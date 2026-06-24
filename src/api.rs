// RunningHub OpenAPI 客户端（阻塞式，跑在后台线程；UI 线程绝不调用它）。
// 对齐 rh_batch.py 的 RunningHub 类：
//   · upload_image      上传图片，含 NET_RETRY 网络重试
//   · create_task       提交任务，解析 promptTips.node_errors，识别队列占满需重试
//   · poll_once         轮询一次，区分 完成 / 排队中 / 不确定（容忍若干次）
//   · download          下载结果，含网络重试
//   · account_status    查询账户并发与余额
#![allow(non_snake_case)]

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

pub const BASE: &str = "https://www.runninghub.cn";

/// 网络/接口超时与重试参数（对齐 Python CONFIG）。
#[derive(Clone)]
pub struct RhSettings {
    pub net_retry: u32, // 单次网络调用失败的重试次数（NET_RETRY）
}

impl Default for RhSettings {
    fn default() -> Self {
        Self { net_retry: 4 }
    }
}

#[derive(Clone)]
pub struct RhClient {
    api_key: String,
    http: reqwest::blocking::Client,
    settings: RhSettings,
}

/// 提交任务结果：拿到 taskId，或被并发/队列限制挡住需重试。
pub enum CreateOutcome {
    /// 成功拿到 taskId；node_warnings 为 promptTips 里解析出的节点校验提示（如有）。
    Task {
        task_id: String,
        node_warnings: Option<String>,
    },
    /// 队列/并发占满，调用方应等待后重试。
    Busy(String),
}

/// 单次轮询的三态结果。
pub enum PollState {
    /// 任务完成，返回输出列表。
    Done(Vec<OutputItem>),
    /// 明确仍在排队/运行（或 code=0 但尚无产出），继续等待。
    Pending,
    /// 不确定（可能是短暂的“尚未就绪”，也可能真失败）；调用方累计若干次后再判失败。
    Unknown(String),
}

/// 账户状态（用于开跑前打印并发占用与余额）。
#[derive(Default, Clone)]
pub struct AccountInfo {
    pub current_task_counts: Option<String>,
    pub remain_coins: Option<String>,
}

#[derive(Deserialize)]
struct Resp<T> {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    data: Option<T>,
}

#[derive(Deserialize)]
struct UploadData {
    fileName: String,
}

#[derive(Deserialize)]
struct CreateData {
    taskId: Option<String>,
    #[serde(default)]
    promptTips: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct OutputItem {
    #[serde(default)]
    pub fileUrl: String,
    #[serde(default)]
    #[allow(dead_code)] // 保留以对应接口返回字段，便于将来按类型过滤
    pub fileType: String,
    #[serde(default)]
    pub nodeId: String,
}

/// 把任意 JSON 值转成可读字符串（数字/字符串都支持）。
fn value_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

impl RhClient {
    pub fn new(api_key: impl Into<String>, settings: RhSettings) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180))
            .user_agent("rh-batch/1.0")
            .build()?;
        Ok(Self {
            api_key: api_key.into(),
            http,
            settings,
        })
    }

    /// 通用网络重试：失败后退避再试，最多 net_retry 次（对齐 Python 的退避节奏）。
    fn with_retry<T>(&self, mut f: impl FnMut() -> Result<T>) -> Result<T> {
        let tries = self.settings.net_retry.max(1);
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..tries {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last = Some(e);
                    if attempt + 1 < tries {
                        std::thread::sleep(Duration::from_millis(1500 * (attempt as u64 + 1)));
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("网络请求失败（重试用尽）")))
    }

    /// 上传图片，返回 RunningHub 内部 fileName（形如 api/xxxx.png）。
    pub fn upload_image(&self, path: &Path) -> Result<String> {
        let url = format!("{BASE}/task/openapi/upload");
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.png")
            .to_string();
        let mime = match path
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
        };
        let bytes = std::fs::read(path).map_err(|e| anyhow!("读取图片失败：{e}"))?;

        self.with_retry(|| {
            let part = reqwest::blocking::multipart::Part::bytes(bytes.clone())
                .file_name(fname.clone())
                .mime_str(mime)?;
            let form = reqwest::blocking::multipart::Form::new()
                .text("apiKey", self.api_key.clone())
                .text("fileType", "image")
                .part("file", part);

            let r: Resp<UploadData> = self
                .http
                .post(url.as_str())
                .multipart(form)
                .send()?
                .error_for_status()?
                .json()?;
            if r.code == 0 {
                if let Some(d) = r.data {
                    return Ok(d.fileName);
                }
            }
            Err(anyhow!("上传返回异常 code={} msg={:?}", r.code, r.msg))
        })
    }

    /// 提交任务（nodes 为 nodeInfoList 数组）。
    /// 成功返回 taskId（并附带 promptTips 中的 node_errors 提示）；
    /// 队列/并发占满返回 Busy；其它失败返回 Err。
    pub fn create_task(
        &self,
        workflow_id: &str,
        nodes: &serde_json::Value,
        add_metadata: bool,
    ) -> Result<CreateOutcome> {
        let url = format!("{BASE}/task/openapi/create");
        let body = serde_json::json!({
            "apiKey": self.api_key,
            "workflowId": workflow_id,
            "addMetadata": add_metadata,
            "nodeInfoList": nodes,
        });
        let r: Resp<CreateData> =
            self.with_retry(|| Ok(self.http.post(url.as_str()).json(&body).send()?.json()?))?;

        if r.code == 0 {
            if let Some(d) = r.data {
                if let Some(id) = d.taskId {
                    let warnings = d.promptTips.as_deref().and_then(parse_node_errors);
                    return Ok(CreateOutcome::Task {
                        task_id: id,
                        node_warnings: warnings,
                    });
                }
            }
            return Err(anyhow!("提交成功但无 taskId"));
        }
        let msg = r.msg.unwrap_or_default();
        let up = msg.to_uppercase();
        let busy = ["QUEUE", "MAXED", "RUNNING", "CONCURRENT", "LIMIT", "BUSY"]
            .iter()
            .any(|k| up.contains(k));
        if busy {
            Ok(CreateOutcome::Busy(msg))
        } else {
            Err(anyhow!("提交失败 code={} msg={}", r.code, msg))
        }
    }

    /// 查询一次任务状态（含网络重试）。
    pub fn poll_once(&self, task_id: &str) -> Result<PollState> {
        let url = format!("{BASE}/task/openapi/outputs");
        let body = serde_json::json!({ "apiKey": self.api_key, "taskId": task_id });
        let v: serde_json::Value =
            self.with_retry(|| Ok(self.http.post(url.as_str()).json(&body).send()?.json()?))?;

        let code = v.get("code").and_then(|x| x.as_i64()).unwrap_or(-1);
        let msg = v
            .get("msg")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_uppercase();
        let data = v.get("data");

        // 完成：code=0 且 data 为非空数组
        if code == 0 {
            if let Some(arr) = data.and_then(|d| d.as_array()) {
                if !arr.is_empty() {
                    let items: Vec<OutputItem> =
                        serde_json::from_value(serde_json::Value::Array(arr.clone()))?;
                    return Ok(PollState::Done(items));
                }
            }
            // code=0 但 data 为 null/空：尚未产出，继续等
            return Ok(PollState::Pending);
        }
        // 明确排队中/运行中
        if msg.contains("QUEUED") || msg.contains("RUNNING") {
            return Ok(PollState::Pending);
        }
        // 其它：不确定，交由调用方累计容忍
        Ok(PollState::Unknown(format!(
            "code={code} msg={}",
            v.get("msg").and_then(|x| x.as_str()).unwrap_or("")
        )))
    }

    /// 下载结果文件（含网络重试）。
    pub fn download(&self, url: &str, out: &Path) -> Result<()> {
        let bytes =
            self.with_retry(|| Ok(self.http.get(url).send()?.error_for_status()?.bytes()?))?;
        std::fs::write(out, &bytes).map_err(|e| anyhow!("写文件失败：{e}"))?;
        Ok(())
    }

    /// 查询账户状态：并发占用 currentTaskCounts、余额 remainCoins/remainMoney。
    pub fn account_status(&self) -> AccountInfo {
        let url = format!("{BASE}/uc/openapi/accountStatus");
        let body = serde_json::json!({ "apiKey": self.api_key });
        let res: Result<serde_json::Value> =
            self.with_retry(|| Ok(self.http.post(url.as_str()).json(&body).send()?.json()?));
        let mut info = AccountInfo::default();
        if let Ok(v) = res {
            if let Some(data) = v.get("data") {
                if let Some(c) = data.get("currentTaskCounts") {
                    info.current_task_counts = value_to_string(c);
                }
                let coins = data
                    .get("remainCoins")
                    .or_else(|| data.get("remainMoney"));
                if let Some(c) = coins {
                    info.remain_coins = value_to_string(c);
                }
            }
        }
        info
    }
}

/// 从 promptTips（一个 JSON 字符串）中解析 node_errors，转成可读告警。
fn parse_node_errors(tips: &str) -> Option<String> {
    let t: serde_json::Value = serde_json::from_str(tips).ok()?;
    let errs = t.get("node_errors")?;
    let empty = match errs {
        serde_json::Value::Null => true,
        serde_json::Value::Object(m) => m.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        _ => false,
    };
    if empty {
        None
    } else {
        Some(errs.to_string())
    }
}
