#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
RunningHub 工作流批量自动化处理脚本  (rh_batch.py)

作用：把本地“输入文件夹”里的所有图片，自动上传到 RunningHub 跑你的工作流
     （Flux2-Klein 人像精修 / 高清放大），再把结果图自动下载到“输出文件夹”。
      全程不用开网页、不用手动一张张上传下载。

———————————————— 使用方法（超简单，三步）————————————————
  第 1 步：在下面【参数设置区】填好 4 个值（apiKey / workflowId / 两个文件夹路径）
  第 2 步：把要处理的图片都放进“输入文件夹”
  第 3 步：双击运行本文件（或在终端执行  python rh_batch.py）

———————————————— 首次使用的一次性准备 ————————————————
  · 安装依赖：在命令行执行   pip install requests
  · 把工作流 JSON 导入 RunningHub（我的工作流 → 导入），打开后从网址
    https://www.runninghub.cn/workflow/【这串数字】 复制出 workflowId
  · 头像 → “API 调用”页面 创建并复制 apiKey
"""

import os
import sys
import csv
import json
import time
import argparse
import mimetypes
from concurrent.futures import ThreadPoolExecutor, as_completed

try:
    import requests
except ImportError:
    print("缺少依赖 requests，请先在命令行运行：pip install requests")
    try:
        input("\n按回车键退出…")
    except Exception:
        pass
    sys.exit(1)


# ┌────────────────────────────────────────────────────────────────────────────┐
# │                                                                            │
# │   ★★★  参 数 设 置 区 —— 只 需 修 改 这 一 段 ，填 完 直 接 运 行  ★★★      │
# │                                                                            │
# └────────────────────────────────────────────────────────────────────────────┘

# ===== 必填 4 项 ==============================================================

API_KEY      = "6bf6a7acae4547f1b9b1752d153b5e86"          # 你的 RunningHub apiKey，例如 "0s2d1xxxxxxxxxx2n3mk4"

WORKFLOW_ID  = "2068941881874149378"          # 工作流 ID（网址 /workflow/ 后面那串数字）

INPUT_DIR    = r"D:\ai_work\runninghub_api\input"         # 待处理图片文件夹，例如  r"D:\照片\待处理"
                           # （路径前的 r 不要删；直接把 Windows 路径粘到引号里即可）

OUTPUT_DIR   = r"D:\ai_work\runninghub_api\output"         # 结果输出文件夹，例如    r"D:\照片\已处理"


# ===== 工作流文件（用于自动识别图片输入/结果输出节点，通常不用改）=============

WORKFLOW_JSON = "Flux2-Klein人像精修_高清放大__2_.json"
# 把这个 .json 和本脚本放在同一个文件夹即可；脚本会自动识别 LoadImage / SaveImage 节点。


# ===== 可选项（一般保持默认）=================================================

CONCURRENCY     = 2        # 同时处理几张。按你套餐的并发额度填；基础版填 1。
                           # 额度足够时调大可成倍提速（脚本会自动排队重试）。

OVERWRITE       = False    # False=断点续跑，已处理过的自动跳过（推荐）
                           # True =全部重新处理

PAUSE_ON_FINISH = True     # 运行结束后是否暂停等待按键（双击运行时建议 True，
                           # 这样窗口不会一闪而过；用命令行批处理可设 False）

#  ↑↑↑  以上填完就行，下面的代码不用动  ↑↑↑
# ==============================================================================


# 脚本所在目录（双击运行时用于定位同目录下的工作流 JSON）
try:
    SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
except Exception:
    SCRIPT_DIR = os.getcwd()


# 把上面的简单变量整理成内部配置字典（下方代码使用，无需改动）
CONFIG = {
    "API_KEY":       API_KEY,
    "WORKFLOW_ID":   WORKFLOW_ID,
    "INPUT_DIR":     INPUT_DIR,
    "OUTPUT_DIR":    OUTPUT_DIR,
    "WORKFLOW_JSON": WORKFLOW_JSON,

    "INPUT_NODE_ID":  "642",     # 自动识别（LoadImage）；如需手动指定填字符串如 "642"
    "INPUT_FIELD":    "image",
    "OUTPUT_NODE_ID": None,     # 自动识别（SaveImage）；None=下载全部输出

    # 额外覆盖的节点参数（高级，可选）。例：固定种子保证可复现：
    #   [{"nodeId": "采样器节点id", "fieldName": "seed", "fieldValue": 123456}]
    "EXTRA_NODE_OVERRIDES": [],

    "BASE_URL":          "https://www.runninghub.cn",
    "CONCURRENCY":       CONCURRENCY,
    "POLL_INTERVAL":     5,
    "TASK_TIMEOUT":      1800,
    "CREATE_RETRY":      20,
    "CREATE_RETRY_WAIT": 15,
    "NET_RETRY":         4,
    "ADD_METADATA":      True,
    "OVERWRITE":         OVERWRITE,
    "PAUSE_ON_FINISH":   PAUSE_ON_FINISH,
    "IMAGE_EXTS":        [".png", ".jpg", ".jpeg", ".webp", ".bmp"],
}
# ==============================================================================


def log(msg):
    print(time.strftime("[%H:%M:%S] ") + str(msg), flush=True)


# ----------------------------- 工作流节点自动识别 -----------------------------
def detect_io_nodes(workflow_json_path):
    """从本地 UI 格式工作流 JSON 中自动找出 LoadImage(输入) 与 SaveImage(输出) 节点 id。"""
    with open(workflow_json_path, "r", encoding="utf-8") as f:
        wf = json.load(f)
    nodes = wf.get("nodes", [])

    load_nodes = [str(n["id"]) for n in nodes if n.get("type") == "LoadImage"]
    save_nodes = [str(n["id"]) for n in nodes if n.get("type") == "SaveImage"]

    input_node = load_nodes[0] if len(load_nodes) == 1 else None
    output_node = save_nodes[0] if len(save_nodes) == 1 else None

    log(f"工作流识别：LoadImage 节点={load_nodes or '无'} | SaveImage 节点={save_nodes or '无'}")
    if len(load_nodes) > 1:
        log("⚠ 检测到多个 LoadImage 节点，请用 --input-node-id 指定要替换的那个。")
    if len(save_nodes) > 1:
        log("⚠ 检测到多个 SaveImage 节点，将默认下载全部 SaveImage 输出（或用 --output-node-id 指定）。")
    return input_node, output_node, save_nodes


# --------------------------------- API 封装 -----------------------------------
class RunningHub:
    def __init__(self, cfg):
        self.api_key = cfg["API_KEY"]
        self.base = cfg["BASE_URL"].rstrip("/")
        self.cfg = cfg
        self.sess = requests.Session()
        self.sess.headers.update({"User-Agent": "rh-batch/1.0"})

    def _post_json(self, path, payload, timeout=30):
        url = f"{self.base}{path}"
        last = None
        for attempt in range(self.cfg["NET_RETRY"]):
            try:
                r = self.sess.post(url, json=payload, timeout=timeout)
                return r.json()
            except Exception as e:
                last = e
                time.sleep(1.5 * (attempt + 1))
        raise RuntimeError(f"请求失败 {url}: {last}")

    def upload_image(self, file_path):
        """上传单张图片，返回 RunningHub 内部 fileName（形如 api/xxxx.png）。"""
        url = f"{self.base}/task/openapi/upload"
        fname = os.path.basename(file_path)
        ctype = mimetypes.guess_type(fname)[0] or "image/png"
        last = None
        for attempt in range(self.cfg["NET_RETRY"]):
            try:
                with open(file_path, "rb") as fp:
                    files = {"file": (fname, fp, ctype)}
                    data = {"apiKey": self.api_key, "fileType": "image"}
                    r = self.sess.post(url, data=data, files=files, timeout=120)
                j = r.json()
                if j.get("code") == 0 and j.get("data", {}).get("fileName"):
                    return j["data"]["fileName"]
                raise RuntimeError(f"上传返回异常: {j}")
            except Exception as e:
                last = e
                time.sleep(2 * (attempt + 1))
        raise RuntimeError(f"上传失败 {file_path}: {last}")

    def create_task(self, node_info_list):
        """提交任务。遇到并发/队列占满会自动等待重试。返回 taskId。"""
        path = "/task/openapi/create"
        payload = {
            "apiKey": self.api_key,
            "workflowId": str(self.cfg["WORKFLOW_ID"]),
            "addMetadata": bool(self.cfg["ADD_METADATA"]),
            "nodeInfoList": node_info_list,
        }
        for attempt in range(self.cfg["CREATE_RETRY"]):
            j = self._post_json(path, payload)
            code = j.get("code")
            msg = (j.get("msg") or "")
            data = j.get("data") or {}
            if code == 0 and data.get("taskId"):
                # promptTips 里可能含节点配置错误提示
                tips = data.get("promptTips")
                if tips:
                    try:
                        t = json.loads(tips)
                        if t.get("node_errors"):
                            log(f"⚠ 工作流节点校验提示: {t['node_errors']}")
                    except Exception:
                        pass
                return data["taskId"]
            # 并发占满 / 队列已满：等待后重试
            if any(k in msg.upper() for k in ["QUEUE", "MAXED", "RUNNING", "CONCURRENT", "LIMIT", "BUSY"]):
                log(f"  队列/并发占满（{msg}），{self.cfg['CREATE_RETRY_WAIT']}s 后重试 "
                    f"({attempt+1}/{self.cfg['CREATE_RETRY']})…")
                time.sleep(self.cfg["CREATE_RETRY_WAIT"])
                continue
            raise RuntimeError(f"提交任务失败: code={code} msg={msg}")
        raise RuntimeError("提交任务失败：重试次数用尽（可能并发额度不足，建议降低 --concurrency）")

    def wait_outputs(self, task_id):
        """轮询任务直到完成，返回输出列表 [{fileUrl,nodeId,fileType,...}]。"""
        path = "/task/openapi/outputs"
        payload = {"apiKey": self.api_key, "taskId": str(task_id)}
        deadline = time.time() + self.cfg["TASK_TIMEOUT"]
        unknown_strikes = 0
        while time.time() < deadline:
            j = self._post_json(path, payload, timeout=25)
            code = j.get("code")
            msg = (j.get("msg") or "")
            data = j.get("data")

            if code == 0 and isinstance(data, list) and data:
                return data  # 完成
            if "QUEUED" in msg.upper() or "RUNNING" in msg.upper():
                unknown_strikes = 0
                time.sleep(self.cfg["POLL_INTERVAL"])
                continue
            if code == 0 and (data in (None, [], {})):
                # 还没产出结果，继续等
                time.sleep(self.cfg["POLL_INTERVAL"])
                continue
            # 其它情况：可能是短暂的“尚未就绪”或真失败。容忍几次再判失败。
            unknown_strikes += 1
            if unknown_strikes >= 4:
                raise RuntimeError(f"任务失败/异常: code={code} msg={msg} data={data}")
            time.sleep(self.cfg["POLL_INTERVAL"])
        raise RuntimeError(f"任务超时（>{self.cfg['TASK_TIMEOUT']}s），taskId={task_id}")

    def account_status(self):
        try:
            j = self._post_json("/uc/openapi/accountStatus", {"apiKey": self.api_key}, timeout=15)
            return j.get("data") or {}
        except Exception:
            return {}


# ------------------------------- 下载结果 -------------------------------------
def download(url, out_path, sess):
    for attempt in range(4):
        try:
            r = sess.get(url, timeout=120)
            r.raise_for_status()
            with open(out_path, "wb") as f:
                f.write(r.content)
            return True
        except Exception:
            time.sleep(2 * (attempt + 1))
    return False


def pick_ext_from_url(url, default=".png"):
    base = url.split("?")[0]
    _, ext = os.path.splitext(base)
    return ext if ext else default


# ------------------------------ 处理单张图片 ----------------------------------
def process_one(rh, cfg, img_path, out_dir, sess):
    name = os.path.splitext(os.path.basename(img_path))[0]

    # 断点续跑：已有同名结果(name_rh*.ext)则跳过
    if not cfg["OVERWRITE"]:
        if any(f.startswith(name + "_rh") for f in os.listdir(out_dir)):
            log(f"⏭ 跳过（已存在结果）：{os.path.basename(img_path)}")
            return {"input": img_path, "status": "skipped", "task_id": "", "outputs": ""}

    log(f"▶ 上传：{os.path.basename(img_path)}")
    file_name = rh.upload_image(img_path)

    node_info = [{
        "nodeId": str(cfg["INPUT_NODE_ID"]),
        "fieldName": cfg["INPUT_FIELD"],
        "fieldValue": file_name,
    }]
    node_info.extend(cfg["EXTRA_NODE_OVERRIDES"])

    log(f"  提交任务（注入到节点 {cfg['INPUT_NODE_ID']}）…")
    task_id = rh.create_task(node_info)
    log(f"  taskId={task_id}，等待生成…")

    outputs = rh.wait_outputs(task_id)

    # 过滤：只要指定 SaveImage 节点的输出（若设置了 OUTPUT_NODE_ID）
    target = outputs
    if cfg["OUTPUT_NODE_ID"]:
        filtered = [o for o in outputs if str(o.get("nodeId")) == str(cfg["OUTPUT_NODE_ID"])]
        target = filtered if filtered else outputs  # 找不到就退化为全部

    saved = []
    multi = len(target) > 1
    for i, o in enumerate(target):
        url = o.get("fileUrl")
        if not url:
            continue
        ext = pick_ext_from_url(url)
        suffix = f"_rh{('_'+str(i+1)) if multi else ''}{ext}"
        out_path = os.path.join(out_dir, name + suffix)
        if download(url, out_path, sess):
            saved.append(out_path)
            log(f"  ✓ 已保存：{os.path.basename(out_path)}")
        else:
            log(f"  ✗ 下载失败：{url}")

    return {
        "input": img_path,
        "status": "ok" if saved else "no_output",
        "task_id": task_id,
        "outputs": ";".join(saved),
    }


# ----------------------------------- 主流程 -----------------------------------
def main():
    p = argparse.ArgumentParser(description="RunningHub 工作流批量处理")
    p.add_argument("--api-key")
    p.add_argument("--workflow-id")
    p.add_argument("--input")
    p.add_argument("--output")
    p.add_argument("--workflow-json")
    p.add_argument("--input-node-id")
    p.add_argument("--output-node-id")
    p.add_argument("--concurrency", type=int)
    p.add_argument("--overwrite", action="store_true")
    args = p.parse_args()

    cfg = dict(CONFIG)
    if args.api_key:        cfg["API_KEY"] = args.api_key
    if args.workflow_id:    cfg["WORKFLOW_ID"] = args.workflow_id
    if args.input:          cfg["INPUT_DIR"] = args.input
    if args.output:         cfg["OUTPUT_DIR"] = args.output
    if args.workflow_json:  cfg["WORKFLOW_JSON"] = args.workflow_json
    if args.input_node_id:  cfg["INPUT_NODE_ID"] = args.input_node_id
    if args.output_node_id: cfg["OUTPUT_NODE_ID"] = args.output_node_id
    if args.concurrency:    cfg["CONCURRENCY"] = args.concurrency
    if args.overwrite:      cfg["OVERWRITE"] = True

    # 基本校验：必填项是否填了
    missing = []
    if not str(cfg["API_KEY"]).strip():     missing.append("API_KEY（apiKey）")
    if not str(cfg["WORKFLOW_ID"]).strip(): missing.append("WORKFLOW_ID（工作流ID）")
    if not str(cfg["INPUT_DIR"]).strip():   missing.append("INPUT_DIR（输入文件夹）")
    if not str(cfg["OUTPUT_DIR"]).strip():  missing.append("OUTPUT_DIR（输出文件夹）")
    if missing:
        log("❌ 还有参数没填：" + " 、 ".join(missing))
        log("   请打开本脚本顶部【★ 参数设置区 ★】把它们填好，保存后再运行。")
        sys.exit(1)

    # 定位工作流 JSON：若是相对文件名且当前目录找不到，就到脚本所在目录找
    wf = cfg["WORKFLOW_JSON"]
    if wf and not os.path.isabs(wf) and not os.path.exists(wf):
        cand = os.path.join(SCRIPT_DIR, wf)
        if os.path.exists(cand):
            cfg["WORKFLOW_JSON"] = cand

    # 自动识别节点
    if cfg["WORKFLOW_JSON"] and os.path.exists(cfg["WORKFLOW_JSON"]):
        in_node, out_node, _ = detect_io_nodes(cfg["WORKFLOW_JSON"])
        if not cfg["INPUT_NODE_ID"]:
            cfg["INPUT_NODE_ID"] = in_node
        if not cfg["OUTPUT_NODE_ID"]:
            cfg["OUTPUT_NODE_ID"] = out_node
    else:
        log(f"⚠ 没找到工作流文件 {cfg['WORKFLOW_JSON']}，将无法自动识别节点。")
    if not cfg["INPUT_NODE_ID"]:
        log("❌ 未能确定图片输入节点 id。请把工作流 .json 放到脚本同目录，"
            "或在顶部把 CONFIG 里的 INPUT_NODE_ID 手动填成 \"642\"。")
        sys.exit(1)
    log(f"使用：输入节点={cfg['INPUT_NODE_ID']}  输出节点={cfg['OUTPUT_NODE_ID'] or '全部'}")

    # 收集输入图片
    in_dir = cfg["INPUT_DIR"]
    if not os.path.isdir(in_dir):
        log(f"❌ 输入文件夹不存在：{in_dir}")
        sys.exit(1)
    exts = tuple(e.lower() for e in cfg["IMAGE_EXTS"])
    images = sorted(
        os.path.join(in_dir, f) for f in os.listdir(in_dir)
        if f.lower().endswith(exts) and os.path.isfile(os.path.join(in_dir, f))
    )
    if not images:
        log(f"❌ 输入文件夹里没有图片（支持 {cfg['IMAGE_EXTS']}）：{in_dir}")
        sys.exit(1)

    out_dir = cfg["OUTPUT_DIR"]
    os.makedirs(out_dir, exist_ok=True)
    log(f"共发现 {len(images)} 张图片，输出到：{out_dir}")

    rh = RunningHub(cfg)
    acc = rh.account_status()
    if acc:
        log(f"账户状态：currentTaskCounts={acc.get('currentTaskCounts')} "
            f"remainCoins={acc.get('remainCoins', acc.get('remainMoney',''))}")

    dl_sess = requests.Session()
    dl_sess.headers.update({"User-Agent": "rh-batch/1.0"})

    results = []
    conc = max(1, int(cfg["CONCURRENCY"]))
    log(f"开始处理（并发={conc}）…")

    def worker(path):
        try:
            return process_one(rh, cfg, path, out_dir, dl_sess)
        except Exception as e:
            log(f"  ✗ 处理失败 {os.path.basename(path)}: {e}")
            return {"input": path, "status": f"error: {e}", "task_id": "", "outputs": ""}

    if conc == 1:
        for path in images:
            results.append(worker(path))
    else:
        with ThreadPoolExecutor(max_workers=conc) as ex:
            futs = {ex.submit(worker, path): path for path in images}
            for fut in as_completed(futs):
                results.append(fut.result())

    # 写处理清单
    manifest = os.path.join(out_dir, "_manifest.csv")
    with open(manifest, "w", newline="", encoding="utf-8-sig") as f:
        w = csv.writer(f)
        w.writerow(["input", "status", "task_id", "outputs"])
        for r in results:
            w.writerow([r["input"], r["status"], r["task_id"], r["outputs"]])

    ok = sum(1 for r in results if r["status"] == "ok")
    sk = sum(1 for r in results if r["status"] == "skipped")
    bad = len(results) - ok - sk
    log(f"全部完成：成功 {ok}，跳过 {sk}，失败 {bad}。清单见 {manifest}")


def _pause():
    """双击运行时，结束后暂停等待按键，避免窗口一闪而过。"""
    try:
        if PAUSE_ON_FINISH and sys.stdin and sys.stdin.isatty():
            input("\n处理结束，按回车键关闭窗口…")
    except Exception:
        pass


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\n已手动中断。")
        _pause()
    except SystemExit:
        _pause()
        raise
    except Exception as e:
        import traceback
        traceback.print_exc()
        print(f"\n❌ 运行出错：{e}")
        _pause()
    else:
        _pause()
