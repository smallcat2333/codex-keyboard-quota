"""读取一次当前 Codex 账号额度并写入 TICKTYPE DP-104 静态点阵。"""

import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time
import winreg
from pathlib import Path

try:
    import hid
except ImportError as error:
    raise SystemExit("缺少 hidapi，请执行：python -m pip install hidapi") from error


# DP-104 在公开 VIA 配置中的 USB 标识和厂商 HID 接口标识。
VENDOR_ID = 0xE560
PRODUCT_ID = 0xE104
VENDOR_USAGE_PAGE = 0xFF60
VENDOR_USAGE = 0x61
MATRIX_ROWS = 8
MATRIX_COLS = 24
MATRIX_FRAME_COUNT = 1
MATRIX_FPS = 1
MATRIX_DATA_CHUNK_LENGTH = 25
HID_REPORT_LENGTH = 33
APP_SERVER_TIMEOUT_SECONDS = 15
FIVE_HOUR_WINDOW_MINUTES = 5 * 60
SEVEN_DAY_WINDOW_MINUTES = 7 * 24 * 60
WEEK_RESET_CELL_COUNT = 10
WEEK_RESET_ROWS = 5
WEEK_RESET_COLS = 2
REGISTRY_PATH = r"Software\Smallcat\CodexKeyboardQuota"
REGISTRY_FIVE_HOUR_VALUE = "FiveHourRemaining"
REGISTRY_SEVEN_DAY_VALUE = "SevenDayRemaining"
REGISTRY_WEEK_RESET_CELLS_VALUE = "SevenDayResetCells"
REGISTRY_UNLIMITED_VALUE = "--"
FIVE_HOUR_REFRESH_THRESHOLD = 5
SEVEN_DAY_REFRESH_THRESHOLD = 2
LOW_QUOTA_FORCE_REFRESH_THRESHOLD = 5

# 3×5 点阵字模，适配 DP-104 的 24 列显示区域。
FONT = {
    "0": ("111", "101", "101", "101", "111"),
    "1": ("010", "110", "010", "010", "111"),
    "2": ("110", "001", "010", "100", "111"),
    "3": ("110", "001", "010", "001", "110"),
    "4": ("101", "101", "111", "001", "001"),
    "5": ("111", "100", "110", "001", "110"),
    "6": ("110", "100", "111", "101", "111"),
    "7": ("111", "001", "010", "010", "010"),
    "8": ("111", "101", "111", "101", "111"),
    "9": ("111", "101", "111", "001", "110"),
    "-": ("000", "000", "111", "000", "000"),
    "%": ("101", "001", "010", "100", "101"),
    "|": ("1", "1", "1", "1", "1"),
}

HSV_RED = (0, 255, 255)
HSV_YELLOW = (43, 255, 255)
HSV_GREEN = (85, 255, 255)
HSV_WHITE = (0, 0, 255)
HSV_PINK_PURPLE = (213, 200, 255)


def resolve_codex_command():
    """定位本机 Codex CLI，并返回可直接传给 subprocess 的命令前缀。"""
    configured_path = os.environ.get("CODEX_CLI")
    command_path = configured_path or shutil.which("codex")
    if not command_path:
        raise RuntimeError("找不到 codex CLI，请确认 Codex CLI 已加入 PATH")

    command_file = Path(command_path)
    if command_file.suffix.lower() in {".cmd", ".ps1", ".bat"}:
        node_path = shutil.which("node")
        node_script = (
            command_file.parent
            / "node_modules"
            / "@openai"
            / "codex"
            / "bin"
            / "codex.js"
        )
        if node_path and node_script.is_file():
            return [node_path, str(node_script)]

    return [str(command_file)]


class CodexAppServer:
    """通过本机 app-server 读取当前登录账号的 Codex 限额。"""

    def __init__(self):
        """初始化进程命令、JSON-RPC 消息队列和请求序号。"""
        self.command = resolve_codex_command() + ["app-server", "--stdio"]
        self.process = None
        self.messages = queue.Queue()
        self.reader_thread = None
        self.request_id = 0

    def start(self):
        """启动 app-server，完成协议初始化并复用本机 Codex 登录态。"""
        self.process = subprocess.Popen(
            self.command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            bufsize=1,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
        self.reader_thread = threading.Thread(
            target=self._read_stdout,
            name="codex-app-server-reader",
            daemon=True,
        )
        self.reader_thread.start()

        self.request("initialize", {
            "clientInfo": {
                "name": "codex-keyboard-quota",
                "title": "Codex Keyboard Quota",
                "version": "0.1.0",
            }
        })
        self._send({"method": "initialized"})

    def _read_stdout(self):
        """在后台线程中接收 app-server 的 JSON-RPC 行消息。"""
        if self.process is None or self.process.stdout is None:
            return
        for line in iter(self.process.stdout.readline, ""):
            line = line.strip()
            if line:
                self.messages.put(line)
        self.messages.put(None)

    def _send(self, message):
        """向 app-server 发送一条 JSON-RPC 消息并立即刷新管道。"""
        if self.process is None or self.process.stdin is None:
            raise RuntimeError("app-server 尚未启动")
        self.process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def _wait_response(self, request_id):
        """等待指定请求的响应，并忽略无关通知或其他请求响应。"""
        deadline = time.monotonic() + APP_SERVER_TIMEOUT_SECONDS
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("等待 Codex app-server 响应超时")
            try:
                raw_message = self.messages.get(timeout=remaining)
            except queue.Empty as error:
                raise TimeoutError("等待 Codex app-server 响应超时") from error
            if raw_message is None:
                raise RuntimeError("Codex app-server 已退出")
            try:
                message = json.loads(raw_message)
            except json.JSONDecodeError:
                continue
            if str(message.get("id")) == str(request_id):
                return message

    def request(self, method, params=None):
        """发送请求并返回结果；服务端错误直接抛出，避免显示伪造额度。"""
        self.request_id += 1
        request_id = self.request_id
        self._send({"id": request_id, "method": method, "params": params})
        response = self._wait_response(request_id)
        if response.get("error") is not None:
            raise RuntimeError(
                "Codex app-server 请求失败："
                + json.dumps(response["error"], ensure_ascii=False)
            )
        return response["result"]

    def read_quota_status(self):
        """读取两项额度和周重置时间，返回百分比元组与倒计时格数。"""
        result = self.request("account/rateLimits/read", None)
        rate_limits_by_id = result.get("rateLimitsByLimitId")
        if isinstance(rate_limits_by_id, dict) and rate_limits_by_id.get("codex"):
            snapshot = rate_limits_by_id["codex"]
        else:
            snapshot = result.get("rateLimits")
        if not isinstance(snapshot, dict):
            raise RuntimeError("Codex 响应中没有 rate limits 数据")

        windows_by_duration = {}
        for position in ("primary", "secondary"):
            window = snapshot.get(position)
            if window is None:
                continue
            if not isinstance(window, dict):
                raise RuntimeError(f"Codex {position} 窗口格式错误")
            duration = window.get("windowDurationMins")
            if duration not in {FIVE_HOUR_WINDOW_MINUTES, SEVEN_DAY_WINDOW_MINUTES}:
                raise RuntimeError(f"无法识别 Codex 限额窗口时长：{duration}")
            if duration in windows_by_duration:
                raise RuntimeError(f"Codex 返回重复的限额窗口：{duration} 分钟")
            windows_by_duration[duration] = window

        five_hour = windows_by_duration.get(FIVE_HOUR_WINDOW_MINUTES)
        seven_day = windows_by_duration.get(SEVEN_DAY_WINDOW_MINUTES)
        remaining = (
            None if five_hour is None else self._remaining_percent(five_hour, "5 小时"),
            None if seven_day is None else self._remaining_percent(seven_day, "周"),
        )
        seven_day_resets_at = None if seven_day is None else seven_day.get("resetsAt")
        return remaining, calculate_week_reset_cells(seven_day_resets_at)

    @staticmethod
    def _remaining_percent(window, window_name):
        """把服务端窗口的已用百分比转换为整数剩余百分比。"""
        used_percent = window.get("usedPercent")
        if not isinstance(used_percent, (int, float)):
            raise RuntimeError(f"Codex {window_name}窗口没有 usedPercent")
        return max(0, min(100, int(round(100 - used_percent))))

    def close(self):
        """关闭 app-server 进程和输入输出管道。"""
        if self.process is None:
            return
        if self.process.stdin is not None:
            self.process.stdin.close()
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.process = None


class Dp104Keyboard:
    """通过公开 VIA 兼容 HID 协议更新 DP-104 的静态自定义帧。"""

    def __init__(self):
        """枚举并打开 DP-104 的厂商 HID 接口。"""
        devices = hid.enumerate(VENDOR_ID, PRODUCT_ID)
        device_info = next(
            device
            for device in devices
            if device.get("usage_page") == VENDOR_USAGE_PAGE
            and device.get("usage") == VENDOR_USAGE
        )
        self.device = hid.device()
        self.device.open_path(device_info["path"])

    def _send_command(self, command, arguments):
        """发送 33 字节 VIA HID 报文，并校验键盘回显。"""
        logical_report = [0, command, *arguments]
        if len(logical_report) > HID_REPORT_LENGTH:
            raise ValueError("HID 报文超过 DP-104 的 32 字节数据区")
        report = logical_report + [0] * (HID_REPORT_LENGTH - len(logical_report))
        written = self.device.write(report)
        if written != HID_REPORT_LENGTH:
            raise RuntimeError(f"DP-104 HID 写入长度异常：{written}")

        response = list(self.device.read(64, timeout_ms=1000))
        if response and response[0] == 0 and len(response) >= 2:
            response = response[1:]
        expected = logical_report[1:]
        if response[:len(expected)] != expected:
            raise RuntimeError("DP-104 HID 响应校验失败，请关闭网页驱动后重试")
        return response

    def write_custom_frame(self, frame):
        """按单帧配置重载并写入 DP-104 的 8×24 HSV 点阵数据。"""
        expected_length = MATRIX_ROWS * MATRIX_COLS * 3
        if len(frame) != expected_length:
            raise ValueError(f"自定义帧长度必须为 {expected_length} 字节")
        self._send_command(
            209,
            [48, MATRIX_FRAME_COUNT, MATRIX_FPS, MATRIX_ROWS, MATRIX_COLS],
        )
        for offset in range(0, len(frame), MATRIX_DATA_CHUNK_LENGTH):
            chunk = frame[offset:offset + MATRIX_DATA_CHUNK_LENGTH]
            self._send_command(
                209,
                [49, *encode_uint32(offset), len(chunk), *chunk],
            )

    def close(self):
        """关闭 DP-104 HID 句柄。"""
        self.device.close()


def encode_uint32(value):
    """把非负整数编码为协议使用的四字节大端序数组。"""
    if value < 0 or value > 0xFFFFFFFF:
        raise ValueError("协议偏移量超出四字节无符号整数范围")
    return list(value.to_bytes(4, byteorder="big"))


def calculate_week_reset_cells(resets_at, current_time=None):
    """把周窗口剩余时间换算为 0 到 10 个倒计时格。"""
    if resets_at is None:
        return None
    if not isinstance(resets_at, (int, float)):
        raise RuntimeError("Codex 周窗口 resetsAt 格式错误")
    now = time.time() if current_time is None else current_time
    remaining_seconds = max(0, resets_at - now)
    seconds_per_cell = SEVEN_DAY_WINDOW_MINUTES * 60 / WEEK_RESET_CELL_COUNT
    rounded_cells = int(remaining_seconds / seconds_per_cell + 0.5)
    return min(WEEK_RESET_CELL_COUNT, rounded_cells)


def read_recorded_status():
    """从当前用户注册表读取上次成功显示的额度与倒计时格数。"""
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, REGISTRY_PATH) as key:
            five_hour_value = winreg.QueryValueEx(
                key,
                REGISTRY_FIVE_HOUR_VALUE,
            )[0]
            seven_day_value = winreg.QueryValueEx(
                key,
                REGISTRY_SEVEN_DAY_VALUE,
            )[0]
            reset_cells_value = winreg.QueryValueEx(
                key,
                REGISTRY_WEEK_RESET_CELLS_VALUE,
            )[0]
    except FileNotFoundError:
        return None

    five_hour = (
        None
        if five_hour_value == REGISTRY_UNLIMITED_VALUE
        else int(five_hour_value)
    )
    seven_day = (
        None
        if seven_day_value == REGISTRY_UNLIMITED_VALUE
        else int(seven_day_value)
    )
    reset_cells = (
        None
        if reset_cells_value == REGISTRY_UNLIMITED_VALUE
        else int(reset_cells_value)
    )
    return (five_hour, seven_day), reset_cells


def write_recorded_status(status):
    """把本次成功显示的额度与倒计时格数写入当前用户注册表。"""
    remaining, reset_cells = status
    five_hour, seven_day = remaining
    five_hour_value = (
        REGISTRY_UNLIMITED_VALUE if five_hour is None else str(five_hour)
    )
    seven_day_value = (
        REGISTRY_UNLIMITED_VALUE if seven_day is None else str(seven_day)
    )
    reset_cells_value = (
        REGISTRY_UNLIMITED_VALUE if reset_cells is None else str(reset_cells)
    )
    with winreg.CreateKeyEx(
        winreg.HKEY_CURRENT_USER,
        REGISTRY_PATH,
        access=winreg.KEY_SET_VALUE,
    ) as key:
        winreg.SetValueEx(
            key,
            REGISTRY_FIVE_HOUR_VALUE,
            0,
            winreg.REG_SZ,
            five_hour_value,
        )
        winreg.SetValueEx(
            key,
            REGISTRY_SEVEN_DAY_VALUE,
            0,
            winreg.REG_SZ,
            seven_day_value,
        )
        winreg.SetValueEx(
            key,
            REGISTRY_WEEK_RESET_CELLS_VALUE,
            0,
            winreg.REG_SZ,
            reset_cells_value,
        )


def should_refresh_display(current_status, recorded_status):
    """仅按额度百分比变化判断是否刷新，沙漏只随额度刷新一并更新。"""
    if recorded_status is None:
        return True

    current, _ = current_status
    recorded, _ = recorded_status
    current_five_hour, current_seven_day = current
    recorded_five_hour, recorded_seven_day = recorded
    if (
        current_five_hour is not None
        and current_five_hour < LOW_QUOTA_FORCE_REFRESH_THRESHOLD
        and current_five_hour != recorded_five_hour
    ) or (
        current_seven_day is not None
        and current_seven_day < LOW_QUOTA_FORCE_REFRESH_THRESHOLD
        and current_seven_day != recorded_seven_day
    ):
        return True

    if current_five_hour is None or recorded_five_hour is None:
        five_hour_changed = current_five_hour is not recorded_five_hour
    else:
        five_hour_changed = (
            abs(current_five_hour - recorded_five_hour)
            >= FIVE_HOUR_REFRESH_THRESHOLD
        )
    if current_seven_day is None or recorded_seven_day is None:
        seven_day_changed = current_seven_day is not recorded_seven_day
    else:
        seven_day_changed = (
            abs(current_seven_day - recorded_seven_day)
            >= SEVEN_DAY_REFRESH_THRESHOLD
        )
    return five_hour_changed or seven_day_changed


def build_message(remaining):
    """按用户指定格式生成“5 小时剩余|周剩余”的 ASCII 文本。"""
    five_hour, weekly = remaining
    return f"{format_display_percent(five_hour)}|{format_display_percent(weekly)}%"


def quota_color(remaining_percent):
    """按剩余百分比选择数字颜色；无限额占位符使用绿色。"""
    if remaining_percent is None:
        return HSV_GREEN
    if remaining_percent >= 60:
        return HSV_GREEN
    if remaining_percent >= 20:
        return HSV_YELLOW
    return HSV_RED


def format_display_percent(percent):
    """把额度格式化为两位数；无窗口显示 --，100% 按 99 显示。"""
    if percent is None:
        return "--"
    return f"{min(percent, 99):02d}"


def build_static_frame(remaining, reset_cells):
    """渲染“5h|周|重置沙漏”的 8×24 彩色静态点阵帧。"""
    if reset_cells is not None and not 0 <= reset_cells <= WEEK_RESET_CELL_COUNT:
        raise ValueError("周重置倒计时格数必须在 0 到 10 之间")

    five_hour, weekly = remaining
    colored_segments = (
        (format_display_percent(five_hour), quota_color(five_hour)),
        ("|", HSV_WHITE),
        (format_display_percent(weekly), quota_color(weekly)),
        ("|", HSV_WHITE),
    )
    glyphs = []
    for text, color in colored_segments:
        for index, character in enumerate(text):
            spacing = 2 if index + 1 < len(text) else 1
            glyphs.append((FONT[character], color, spacing))
    text_width = (
        sum(len(glyph[0][0]) for glyph in glyphs)
        + sum(item[2] for item in glyphs[:-1])
    )
    layout_width = text_width + 1 + WEEK_RESET_COLS
    if layout_width > MATRIX_COLS:
        raise ValueError("额度与倒计时超过 DP-104 静态点阵宽度")

    frame = [0] * (MATRIX_ROWS * MATRIX_COLS * 3)
    start_x = (MATRIX_COLS - layout_width) // 2
    start_y = (MATRIX_ROWS - 5) // 2
    current_x = start_x
    for glyph, color, spacing in glyphs:
        for glyph_y, glyph_row in enumerate(glyph):
            for glyph_x, pixel in enumerate(glyph_row):
                if pixel == "1":
                    pixel_index = (start_y + glyph_y) * MATRIX_COLS + current_x + glyph_x
                    frame[pixel_index * 3:pixel_index * 3 + 3] = color
        current_x += len(glyph[0]) + spacing

    lit_cells = 0 if reset_cells is None else reset_cells
    disappeared_cells = WEEK_RESET_CELL_COUNT - lit_cells
    reset_start_x = start_x + text_width + 1
    for index in range(disappeared_cells, WEEK_RESET_CELL_COUNT):
        reset_y = start_y + index // WEEK_RESET_COLS
        reset_x = reset_start_x + index % WEEK_RESET_COLS
        pixel_index = (reset_y * MATRIX_COLS + reset_x) * 3
        frame[pixel_index:pixel_index + 3] = HSV_PINK_PURPLE
    return frame


def parse_args():
    """解析脚本运行参数。"""
    parser = argparse.ArgumentParser(
        description="把当前 Codex 额度写入 TICKTYPE DP-104 静态点阵，执行一次即退出。"
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="只读取并打印额度，不写入键盘",
    )
    return parser.parse_args()


def main():
    """查询额度，仅在变化超过阈值时更新键盘并记录显示值。"""
    args = parse_args()
    app_server = CodexAppServer()
    keyboard = None
    try:
        app_server.start()
        status = app_server.read_quota_status()
        remaining, reset_cells = status
        message = build_message(remaining)
        if args.dry_run:
            print(message, flush=True)
            return 0

        recorded_status = read_recorded_status()
        if not should_refresh_display(status, recorded_status):
            print(f"{message} [skip]", flush=True)
            return 0

        frame = build_static_frame(remaining, reset_cells)
        keyboard = Dp104Keyboard()
        keyboard.write_custom_frame(frame)
        write_recorded_status(status)
        print(message, flush=True)
        return 0
    except Exception as error:
        print(f"更新失败：{error}", file=sys.stderr, flush=True)
        return 1
    except KeyboardInterrupt:
        print("已停止额度显示", flush=True)
        return 0
    finally:
        if keyboard is not None:
            keyboard.close()
        app_server.close()


if __name__ == "__main__":
    raise SystemExit(main())
