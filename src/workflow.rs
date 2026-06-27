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

/// 工作流里检测到的一个可调参数（来自 ComfyUI 节点的 widgets_values）。
/// 调用方据此渲染表单控件，用户改动后转回 nodeInfoList 覆盖项。
#[derive(Clone)]
pub struct DetectedParam {
    pub node_id: String,
    pub node_type: String,
    pub field: String,            // nodeInfoList 用的 fieldName（如 seed/text/scale_by）
    pub label: String,            // 给人看的中文标签
    pub value: Value,            // 工作流里的当前值（作为编辑初值）
}

/// 解析工作流，提取常见可调参数。仅覆盖字段名稳定、语义明确的节点类型，
/// 避免猜错字段名导致跑坏用户工作流。
pub fn detect_params(json_path: &Path) -> Result<Vec<DetectedParam>> {
    let text = std::fs::read_to_string(json_path)
        .map_err(|e| anyhow!("无法读取工作流文件 {}：{e}", json_path.display()))?;
    let wf: Value = serde_json::from_str(&text).map_err(|e| anyhow!("工作流 JSON 解析失败：{e}"))?;
    let nodes = wf
        .get("nodes")
        .and_then(|n| n.as_array())
        .ok_or_else(|| anyhow!("不是有效的工作流 JSON（缺少 nodes 字段）"))?;

    // node_type -> [(widgets_values 下标, fieldName, 中文标签)]
    // 下标依据 ComfyUI 各节点 widgets_values 的固定排列。
    let map: &[(&str, &[(usize, &str, &str)])] = &[
        ("KSampler", &[(0, "seed", "种子"), (2, "steps", "步数"), (3, "cfg", "CFG"), (6, "denoise", "重绘幅度")]),
        ("KSamplerAdvanced", &[(1, "noise_seed", "种子"), (3, "steps", "步数"), (4, "cfg", "CFG")]),
        ("CLIPTextEncode", &[(0, "text", "提示词")]),
        ("ImageScaleBy", &[(1, "scale_by", "放大倍数")]),
    ];

    let mut out = Vec::new();
    for n in nodes {
        let ty = n.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let Some((_, specs)) = map.iter().find(|(t, _)| *t == ty) else { continue };
        let id = match n.get("id") {
            Some(Value::Number(num)) => num.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let Some(wv) = n.get("widgets_values").and_then(|w| w.as_array()) else { continue };
        for (idx, field, label) in *specs {
            let Some(v) = wv.get(*idx) else { continue };
            // 跳过不可编辑的占位（如 null）。
            if v.is_null() {
                continue;
            }
            out.push(DetectedParam {
                node_id: id.clone(),
                node_type: ty.to_string(),
                field: field.to_string(),
                label: label.to_string(),
                value: v.clone(),
            });
        }
    }
    Ok(out)
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
