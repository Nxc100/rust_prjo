#![allow(dead_code)]
//! 4sapi 出图后端（Workflow C「人物入景」用）：`gpt-image-2` 双图编辑。
//!
//! 端点：`POST https://4sapi.com/v1/images/edits`，multipart/form-data。
//! 契约（tips/图片编辑edits、tips/gpt-image-2）：
//!   - 头：`Authorization: Bearer sk-...`、`Accept: application/json`
//!   - 字段：`model=gpt-image-2`、`image[]`(可多张：图A人物 + 图B plate)、`prompt`、`quality`、`size`
//!   - 响应：`{created, data:[{url 或 b64_json}]}`（edits 多回 url，generations 默认 base64，两者都兼容）
//!
//! 错误纪律（交接文档 §4.1 纯 API 命门）：瞬时错(超时/429/502/网络)自动重试；
//! 持久错(503 渠道无货 / 400 / 401 / 余额 0)**响亮报错中止**，绝不静默吞当成功。

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use std::path::Path;
use std::time::Duration;

pub const BASE: &str = "https://4sapi.com";
pub const MODEL: &str = "gpt-image-2";
/// 视觉判景别用的模型（4sapi `/v1/messages` Claude 原生格式）。
pub const VISION_MODEL: &str = "claude-sonnet-4-6";

const SHOT_PROMPT: &str = "你是婚纱样片的「景别 + 人数」判定助手。只看这张人物原片本身，严格只输出一行 JSON、不要任何解释或多余文字：\n{\"shot\":\"full|medium|close|closeup\",\"subjects\":1或2}\n【景别 shot】full=全身(鞋/脚踝可见，或裙摆及地完整入画)；medium=中景(膝~腰之间、脚不入镜，七分身也算 medium)；close=近景(胸口~腋下、头肩胸占满画面)；closeup=特写(肩颈以上、脸占画面比例很大)。景别拿不准时取更「远」的那档。\n【人数 subjects】只数画面里的婚纱**主体人物**：1=单人(只有一位新娘或新郎)，2=两人(一对新人)。背景路人、只露出的一只手或胳膊、镜子里的倒影、人形道具或画像**都不算人**。拿不准时按 1(单人)——宁可少数，绝不凭空多数出一个人。";

#[derive(Clone)]
pub struct FsClient {
    key: String,
    base: String,
    http: reqwest::blocking::Client,
    retry: u32, // 瞬时错重试次数
}

/// 4sapi 账户额度（`/api/user/self`）。quota 为剩余额度原始值。
#[derive(Default, Clone)]
pub struct FsAccount {
    pub quota: i64,
    pub used_quota: i64,
    pub username: String,
    pub group: String,
}
impl FsAccount {
    /// new-api 默认 500000 quota ≈ $1（不同部署可能不同，仅作估算展示）。
    pub fn remain_usd(&self) -> f64 {
        self.quota as f64 / 500000.0
    }
    pub fn used_usd(&self) -> f64 {
        self.used_quota as f64 / 500000.0
    }
}

impl FsClient {
    pub fn new(key: impl Into<String>) -> Result<Self> {
        Self::with_base(key, BASE, 3)
    }

    pub fn with_base(key: impl Into<String>, base: impl Into<String>, retry: u32) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300)) // image2 出图可能数十秒到数分钟
            .user_agent("visual-coast-batch/1.0")
            .build()?;
        Ok(Self {
            key: key.into(),
            base: base.into().trim_end_matches('/').to_string(),
            http,
            retry: retry.max(1),
        })
    }

    /// 双图编辑出图：图A人物 + 图B plate → 解码后的图片字节（png/jpeg/webp 原始字节）。
    /// `size` 形如 1024x1536（用 cmode::size_for_aspect 按原片比例选）；`quality` 一般 high。
    pub fn edit_dual(
        &self,
        person: &Path,
        plate: &Path,
        prompt: &str,
        size: &str,
        quality: &str,
    ) -> Result<Vec<u8>> {
        self.edit_multi(&[person, plate], prompt, size, quality)
    }

    /// 通用：一张或多张 image[] 上传。第一张通常是人物（锁脸主体）。
    pub fn edit_multi(
        &self,
        images: &[&Path],
        prompt: &str,
        size: &str,
        quality: &str,
    ) -> Result<Vec<u8>> {
        if images.is_empty() {
            bail!("至少要一张图片");
        }
        // 预读所有图片字节（重试时复用，避免反复读盘）。
        let mut loaded: Vec<(String, &'static str, Vec<u8>)> = Vec::new();
        for p in images {
            let bytes = std::fs::read(p).map_err(|e| anyhow!("读图失败 {}：{e}", p.display()))?;
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("image.png")
                .to_string();
            loaded.push((fname, mime_of(p), bytes));
        }

        let url = format!("{}/v1/images/edits", self.base);
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..self.retry {
            // multipart 每次重建（Part 不可复用）。
            let mut form = reqwest::blocking::multipart::Form::new()
                .text("model", MODEL.to_string())
                .text("prompt", prompt.to_string())
                .text("size", size.to_string())
                .text("quality", quality.to_string());
            for (fname, mime, bytes) in &loaded {
                let part = reqwest::blocking::multipart::Part::bytes(bytes.clone())
                    .file_name(fname.clone())
                    .mime_str(mime)?;
                form = form.part("image[]", part);
            }

            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.key))
                .header("Accept", "application/json")
                .multipart(form)
                .send();

            match resp {
                Err(e) => {
                    // 网络/超时 = 瞬时错，退避重试。
                    last_err = Some(anyhow!("网络错误：{e}"));
                }
                Ok(r) => {
                    let status = r.status();
                    let code = status.as_u16();
                    let body = r.text().unwrap_or_default();
                    if status.is_success() {
                        return decode_image_payload(&body)
                            .and_then(|opt| opt.ok_or_else(|| persistent_from_body(&body)));
                    }
                    // 非 2xx：按 OpenAI 语义分类（429/502/500 瞬时；503/400/401/403 持久）。
                    if is_transient(code) {
                        last_err = Some(anyhow!("上游瞬时错 HTTP {code}：{}", brief(&body)));
                    } else {
                        // 持久错（含余额 0 / 渠道无货）：立即中止，绝不重试、绝不吞。
                        bail!("4sapi 持久错 HTTP {code}（不重试，请查余额/渠道/参数）：{}", brief(&body));
                    }
                }
            }
            // 退避后重试（1.5s、3s、…）
            if attempt + 1 < self.retry {
                std::thread::sleep(Duration::from_millis(1500 * (attempt as u64 + 1)));
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("出图失败：重试用尽")))
    }

    /// 视觉判景别 + 人数（claude-sonnet-4-6，4sapi `/v1/messages` Claude 原生格式）。
    /// 判错不致命（人工 QC 兜底、可手改）；失败时上层自行兜底默认值。
    pub fn judge_shot(&self, person: &Path) -> Result<(String, u8)> {
        // 缩小后 base64（景别判定不需要全分辨率，省流省钱、避免超图限）。
        let img = image::ImageReader::open(person)
            .map_err(|e| anyhow!("打开图片失败：{e}"))?
            .with_guessed_format()
            .map_err(|e| anyhow!("识别图片格式失败：{e}"))?
            .decode()
            .map_err(|e| anyhow!("解码图片失败：{e}"))?;
        let small = img.thumbnail(1024, 1024);
        let mut cur = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(small.to_rgb8())
            .write_to(&mut cur, image::ImageFormat::Jpeg)
            .map_err(|e| anyhow!("编码缩略图失败：{e}"))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(cur.get_ref());
        let body = serde_json::json!({
            "model": VISION_MODEL,
            "max_tokens": 100,
            "temperature": 0,
            "messages": [{ "role": "user", "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": b64 } },
                { "type": "text", "text": SHOT_PROMPT }
            ]}]
        });
        let url = format!("{}/v1/messages", self.base);
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..self.retry {
            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.key))
                .header("Accept", "application/json")
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send();
            match resp {
                Err(e) => last = Some(anyhow!("网络错误：{e}")),
                Ok(r) => {
                    let code = r.status().as_u16();
                    let text = r.text().unwrap_or_default();
                    if (200..300).contains(&code) {
                        return parse_shot(&text);
                    } else if is_transient(code) {
                        last = Some(anyhow!("判景别瞬时错 HTTP {code}：{}", brief(&text)));
                    } else {
                        bail!("判景别持久错 HTTP {code}：{}", brief(&text));
                    }
                }
            }
            if attempt + 1 < self.retry {
                std::thread::sleep(Duration::from_millis(1200 * (attempt as u64 + 1)));
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("判景别失败：重试用尽")))
    }

    /// 查询账户额度（`GET /api/user/self`）。该面板接口需 4sapi**系统访问令牌**（非 sk- 模型密钥），
    /// 可选带 `New-Api-User` 用户 ID。token 留空则退回用 sk- 密钥（多半会失败、给出引导）。
    /// 兼容两种鉴权头（裸 token / Bearer）。也用作「测试连接」：成功即令牌可用。
    pub fn account(&self, token: &str, user_id: &str) -> Result<FsAccount> {
        let token = if token.trim().is_empty() {
            self.key.clone()
        } else {
            token.trim().to_string()
        };
        let uid = user_id.trim();
        let url = format!("{}/api/user/self", self.base);
        let mut last = String::new();
        for bearer in [false, true] {
            let auth = if bearer {
                format!("Bearer {token}")
            } else {
                token.clone()
            };
            let mut req = self
                .http
                .get(&url)
                .header("Authorization", auth)
                .header("Accept", "application/json");
            if !uid.is_empty() {
                req = req.header("New-Api-User", uid.to_string());
            }
            match req.send() {
                Err(e) => last = format!("网络错误：{e}"),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().unwrap_or_default();
                    if status.is_success() {
                        let v: serde_json::Value = serde_json::from_str(&text)
                            .map_err(|e| anyhow!("解析额度响应失败：{e}；{}", brief(&text)))?;
                        if v.get("success").and_then(|x| x.as_bool()) == Some(true) {
                            let d = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                            let geti = |k: &str| d.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                            let gets = |k: &str| {
                                d.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
                            };
                            let username = {
                                let dn = gets("display_name");
                                if dn.is_empty() { gets("username") } else { dn }
                            };
                            return Ok(FsAccount {
                                quota: geti("quota"),
                                used_quota: geti("used_quota"),
                                username,
                                group: gets("group"),
                            });
                        } else {
                            last = v
                                .get("message")
                                .and_then(|x| x.as_str())
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| brief(&text));
                        }
                    } else {
                        last = format!("HTTP {}：{}", status.as_u16(), brief(&text));
                    }
                }
            }
        }
        bail!("查询额度失败：{last}（若 sk- 密钥不通，该接口可能需面板「访问令牌 / New-Api-User」）");
    }
}

/// 从视觉模型响应里解析 {shot, subjects}。兼容 Anthropic(content[].text) 与 OpenAI(choices[].message.content)。
fn parse_shot(text: &str) -> Result<(String, u8)> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| anyhow!("解析视觉响应失败：{e}；{}", brief(text)))?;
    let answer = v
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.iter().find_map(|b| b.get("text").and_then(|t| t.as_str())))
        .or_else(|| v.pointer("/choices/0/message/content").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    // 抠出 JSON 对象再解析
    if let (Some(s), Some(e)) = (answer.find('{'), answer.rfind('}')) {
        if e > s {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&answer[s..=e]) {
                let shot = j.get("shot").and_then(|x| x.as_str()).unwrap_or("").to_lowercase();
                let shot = match shot.as_str() {
                    "full" | "medium" | "close" | "closeup" => shot,
                    _ => "full".to_string(),
                };
                let subj = if j.get("subjects").and_then(|x| x.as_u64()) == Some(1) { 1u8 } else { 2u8 };
                return Ok((shot, subj));
            }
        }
    }
    // 兜底：从文本关键词猜
    let a = answer.to_lowercase();
    let shot = if a.contains("closeup") {
        "closeup"
    } else if a.contains("close") {
        "close"
    } else if a.contains("medium") {
        "medium"
    } else {
        "full"
    };
    Ok((shot.to_string(), 2))
}

fn mime_of(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// 429/500/502 视为瞬时（可重试）；其余非 2xx 视为持久。
fn is_transient(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 408)
}

fn brief(body: &str) -> String {
    let t = body.trim();
    if t.len() > 300 {
        format!("{}…", &t[..t.char_indices().nth(300).map(|(i, _)| i).unwrap_or(t.len())])
    } else {
        t.to_string()
    }
}

fn persistent_from_body(body: &str) -> anyhow::Error {
    anyhow!("4sapi 返回 200 但无可用图片数据（data 空/格式异常）：{}", brief(body))
}

/// 解析成功响应，取 data[0] 的 b64_json 或 url，落成图片字节。
/// 返回 Ok(None) 表示 200 但 data 里没有可用图片（交给上层报持久错）。
fn decode_image_payload(body: &str) -> Result<Option<Vec<u8>>> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| anyhow!("解析出图响应失败：{e}；原文：{}", brief(body)))?;
    let Some(item) = v.get("data").and_then(|d| d.as_array()).and_then(|a| a.first()) else {
        return Ok(None);
    };
    // 优先 b64_json；否则 url 下载。
    if let Some(b64) = item.get("b64_json").and_then(|x| x.as_str()) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| anyhow!("b64_json 解码失败：{e}"))?;
        return Ok(Some(bytes));
    }
    if let Some(u) = item.get("url").and_then(|x| x.as_str()) {
        let bytes = download(u)?;
        return Ok(Some(bytes));
    }
    Ok(None)
}

fn download(url: &str) -> Result<Vec<u8>> {
    let c = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let r = c.get(url).send()?.error_for_status()?;
    Ok(r.bytes()?.to_vec())
}

/// 读 exe 同目录（或当前目录）的 key.txt 取密钥（首行）；找不到返回 None。
pub fn read_key_file() -> Option<String> {
    let (k, _, _) = read_credentials();
    if k.is_empty() { None } else { Some(k) }
}

/// 取「label：value」里冒号（半角/全角）之后的值；无冒号则整行。
fn after_colon(s: &str) -> String {
    let s = s.trim();
    for (i, c) in s.char_indices() {
        if c == ':' || c == '：' {
            return s[i + c.len_utf8()..].trim().to_string();
        }
    }
    s.to_string()
}

/// 从 exe 同目录/当前目录的 key.txt 读凭据：
///   行1 = sk- 出图密钥；行2 = 系统访问令牌（可带「系统令牌：」前缀）；行3 = 用户ID（可带「用户ID：」前缀）。
/// 返回 (sk_key, access_token, user_id)，缺的为空串。
pub fn read_credentials() -> (String, String, String) {
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("key.txt"))),
        Some(std::path::PathBuf::from("key.txt")),
    ];
    for c in candidates.into_iter().flatten() {
        if let Ok(s) = std::fs::read_to_string(&c) {
            let lines: Vec<&str> = s.lines().collect();
            let key = lines.first().map(|l| after_colon(l)).unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            let token = lines.get(1).map(|l| after_colon(l)).unwrap_or_default();
            let uid: String = lines
                .get(2)
                .map(|l| after_colon(l))
                .unwrap_or_default()
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            return (key, token, uid);
        }
    }
    (String::new(), String::new(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(is_transient(429));
        assert!(is_transient(502));
        assert!(!is_transient(503)); // 渠道无货 = 持久
        assert!(!is_transient(400));
        assert!(!is_transient(401));
    }

    #[test]
    fn decode_b64_payload() {
        // {"data":[{"b64_json":"aGVsbG8="}]} → "hello"
        let body = r#"{"created":1,"data":[{"b64_json":"aGVsbG8="}]}"#;
        let out = decode_image_payload(body).unwrap().unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn decode_empty_data_is_none() {
        assert!(decode_image_payload(r#"{"created":1,"data":[]}"#).unwrap().is_none());
    }

    // 真连 4sapi 出一张（验证 auth/端点/multipart/响应解析/解码全链路）。
    // 默认 #[ignore]，不在常规 cargo test 跑（要花钱、要网络）。手动：
    //   $env:FOURSAPI_KEY=(Get-Content key.txt -First 1); $env:FS_PLATE="...\bbsh_open01.jpg"
    //   cargo test --release foursapi::tests::live_edit_smoke -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_edit_smoke() {
        let key = std::env::var("FOURSAPI_KEY").expect("设 FOURSAPI_KEY 环境变量");
        let plate = std::env::var("FS_PLATE").expect("设 FS_PLATE=一张测试图路径");
        let p = std::path::PathBuf::from(&plate);
        let c = FsClient::new(key).unwrap();
        // 用同一张图当 image[]，低质量小图，仅验证链路连通（省额度）。
        let bytes = c
            .edit_multi(
                &[&p],
                "将这张参考场景图轻微增强为更清晰自然的写实草坪外景照片，真实摄影风格，不要添加文字水印。",
                "1024x1024",
                "low",
            )
            .expect("出图失败");
        assert!(bytes.len() > 1000, "返回字节过小，疑似不是图片：{}", bytes.len());
        let out = std::env::temp_dir().join("fs_smoke_out.png");
        std::fs::write(&out, &bytes).unwrap();
        eprintln!("✅ 出图成功，{} 字节 → {}", bytes.len(), out.display());
    }

    // 真连查额度（测试连接）。默认 #[ignore]。
    //   $env:FOURSAPI_KEY=...; cargo test foursapi::tests::live_account -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_account() {
        let key = std::env::var("FOURSAPI_KEY").expect("设 FOURSAPI_KEY");
        // 额度需系统访问令牌：设 FS_ACCESS_TOKEN（+可选 FS_USER_ID）。留空则用 sk-（多半失败）。
        let token = std::env::var("FS_ACCESS_TOKEN").unwrap_or_default();
        let uid = std::env::var("FS_USER_ID").unwrap_or_default();
        let c = FsClient::new(key).unwrap();
        let a = c.account(&token, &uid).expect("查额度失败");
        eprintln!(
            "✅ 账户 user={} group={} 剩余 ${:.2}(quota {}) 已用 ${:.2}",
            a.username, a.group, a.remain_usd(), a.quota, a.used_usd()
        );
    }

    // 真连视觉判景别（claude-sonnet-4-6）。默认 #[ignore]。
    //   $env:FOURSAPI_KEY=...; $env:FS_PERSON="doc\1V3A3085.JPG"
    //   cargo test foursapi::tests::live_judge_shot -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_judge_shot() {
        let key = std::env::var("FOURSAPI_KEY").expect("设 FOURSAPI_KEY");
        let person = std::env::var("FS_PERSON").expect("设 FS_PERSON");
        let c = FsClient::new(key).unwrap();
        let (shot, subj) = c.judge_shot(std::path::Path::new(&person)).expect("判景别失败");
        eprintln!("✅ 判景别：shot={shot} subjects={subj}");
        assert!(["full", "medium", "close", "closeup"].contains(&shot.as_str()));
        assert!(subj == 1 || subj == 2);
    }

    // 真人端到端（M1 装配 + M2 出图）：cmode 装配双图提示词 → foursapi 双图出图。
    // 默认 #[ignore]。手动：
    //   $env:FOURSAPI_KEY=(Get-Content key.txt -First 1); $env:FS_PERSON="doc\1V3A3085.JPG"
    //   $env:FS_SHOT="medium"; $env:FS_KEY="warm"; $env:FS_SUBJ="2"; $env:FS_SCENE="jiaoshi_01"
    //   cargo test foursapi::tests::live_person_e2e -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_person_e2e() {
        use crate::cmode;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let key = std::env::var("FOURSAPI_KEY").expect("设 FOURSAPI_KEY");
        let person = std::env::var("FS_PERSON").expect("设 FS_PERSON=人物原片路径");
        let shot = std::env::var("FS_SHOT").unwrap_or_else(|_| "full".into());
        let keytone = std::env::var("FS_KEY").unwrap_or_else(|_| "natural".into());
        let subjects: u8 = std::env::var("FS_SUBJ").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
        let scene_id = std::env::var("FS_SCENE").ok();
        let series = std::env::var("FS_SERIES").ok();

        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/wedding");
        let assets = cmode::load_from_dir(&base).expect("加载 assets/wedding 失败");

        let chosen = match scene_id {
            Some(id) => assets
                .scenes
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("catalog 无场景 {id}"))
                .clone(),
            None => {
                let (pick, why) = cmode::auto_pick_scene(
                    &assets.scenes,
                    &shot,
                    series.as_deref(),
                    None,
                    false,
                    &HashMap::new(),
                );
                let id = pick.expect("自动选景失败：该景别无候选");
                eprintln!("自动选景 → {id}（{why}）");
                assets.scenes.iter().find(|s| s.id == id).unwrap().clone()
            }
        };

        let prompt = cmode::assemble_prompt(&assets, &chosen, &shot, &keytone, subjects)
            .expect("提示词装配失败");
        let plate = base.join(cmode::plate_rel_path(&chosen));
        assert!(plate.exists(), "plate 不存在：{}", plate.display());

        let person_p = PathBuf::from(&person);
        let (w, h) = image::image_dimensions(&person_p).unwrap_or((1024, 1536));
        let size = cmode::size_for_aspect(w, h);
        eprintln!(
            "端到端：人物={person}（{w}x{h}）景别={shot} 调色={keytone} 人数={subjects} 场景={} size={size}",
            chosen.id
        );

        let c = FsClient::new(key).unwrap();
        let bytes = c
            .edit_dual(&person_p, &plate, &prompt, size, "high")
            .expect("出图失败");
        assert!(bytes.len() > 5000, "返回字节过小：{}", bytes.len());
        let out = std::env::temp_dir().join("c_e2e_out.png");
        std::fs::write(&out, &bytes).unwrap();
        eprintln!("✅ 端到端出图成功，{} 字节 → {}", bytes.len(), out.display());
    }
}
