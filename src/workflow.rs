// 解析 ComfyUI 工作流(UI 格式 JSON)，自动识别 LoadImage(输入) / SaveImage(输出) 节点。
// 与 rh_batch.py 的 detect_io_nodes 一致：
//   · 唯一 LoadImage → 作为图片输入节点
//   · 唯一 SaveImage → 作为结果输出节点
//   · 多个时不自动选定，交由调用方告警 / 手动指定
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::Path;

#[derive(Default, Clone)]
pub struct IoNodes {
    pub input_node: Option<String>,  // 唯一 LoadImage 时给出
    pub output_node: Option<String>, // 唯一 SaveImage 时给出
    pub all_loads: Vec<String>,
    pub all_saves: Vec<String>,
}

pub fn detect_io_nodes(json_path: &Path) -> Result<IoNodes> {
    let text = std::fs::read_to_string(json_path)
        .map_err(|e| anyhow!("无法读取工作流文件 {}：{e}", json_path.display()))?;
    let wf: Value = serde_json::from_str(&text).map_err(|e| anyhow!("工作流 JSON 解析失败：{e}"))?;
    let nodes = wf
        .get("nodes")
        .and_then(|n| n.as_array())
        .ok_or_else(|| anyhow!("不是有效的工作流 JSON（缺少 nodes 字段）"))?;

    let mut io = IoNodes::default();
    for n in nodes {
        let ty = n.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let id = match n.get("id") {
            Some(Value::Number(num)) => num.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        match ty {
            "LoadImage" => io.all_loads.push(id),
            "SaveImage" => io.all_saves.push(id),
            _ => {}
        }
    }
    io.input_node = (io.all_loads.len() == 1).then(|| io.all_loads[0].clone());
    io.output_node = (io.all_saves.len() == 1).then(|| io.all_saves[0].clone());
    Ok(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_workflow_nodes() {
        // 对齐 rh_batch.py：该工作流应识别出 LoadImage=642、SaveImage=758。
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("Flux2-Klein人像精修+高清放大 (2).json");
        if !p.exists() {
            return; // 工程内未附带 JSON 时跳过
        }
        let io = detect_io_nodes(&p).expect("应能解析工作流");
        assert_eq!(io.input_node.as_deref(), Some("642"));
        assert_eq!(io.output_node.as_deref(), Some("758"));
    }

    #[test]
    fn detects_io_from_inline_json() {
        let tmp = std::env::temp_dir().join("vc_wf_test.json");
        std::fs::write(
            &tmp,
            r#"{"nodes":[{"id":10,"type":"LoadImage"},{"id":"20","type":"SaveImage"},{"id":30,"type":"KSampler"}]}"#,
        )
        .unwrap();
        let io = detect_io_nodes(&tmp).unwrap();
        assert_eq!(io.input_node.as_deref(), Some("10"));
        assert_eq!(io.output_node.as_deref(), Some("20"));
        assert_eq!(io.all_loads, vec!["10".to_string()]);
        let _ = std::fs::remove_file(&tmp);
    }
}
