"""Generate per-scene narration with edge-tts (Microsoft neural voices).

Writes voiceover/<lang>/<scene-id>.mp3, which motion-use picks up through the
brief's "voiceover" block; each scene stretches to fit its line.

edge-tts calls Microsoft Edge's read-aloud service; no license for the audio
has been confirmed. For a published cut, regenerate the same lines with Azure
Speech (same voices) or record them.
"""
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent
VOICES = {"zh": ("zh-CN-YunxiNeural", "+8%"), "en": ("en-US-AndrewMultilingualNeural", "+5%")}

# One line per scene id in brief.json. Every claim is in the README.
SCRIPT = {
    "zh": {
        "hook": "很多 App 没有 API，AI 想帮你点，也点不了。iphone-use 让 AI 直接用你的真 iPhone。",
        "how": "Mac 上跑一个守护进程，通过 USB 驱动手机上的 WebDriverAgent。Agent 只管调接口。",
        "features": "界面读成文字，按编号点击；没点上会直接说；锁着屏，几秒就提醒你解锁。",
        "browser": "远程控制：在浏览器或 iPhone App 里看手机的实时画面，直接点、直接输入。",
        "wireframe": "支付类 App 禁止截屏，截图是一片白。iphone-use 按控件树把按钮和文字画出来。",
        "api": "一步操作：读界面，按编号点，等画面稳定再返回变化，大约四秒。",
        "perf": "快，是一点点抠出来的：界面只读一次，点击前只核对目标位置；画面稳没稳，用截图判断；能缓存的都缓存；每个请求都记下耗时。",
        "perf-numbers": "点击加等待稳定，从八点五秒降到四点二秒；冷启动，从七十五秒降到十三秒。",
        "tokens": "省时间，也省 token：屏幕读成文字，不用每步都看截图；只要变化的部分；点完直接带回结果；一次最多发二十四步。",
        "flows-loop": "做过一次的事，不用每次都让模型重新摸索。录成 flow，审阅后放进官方 flow 源，谁都能直接用。",
        "flows": "比如导出健康数据，一条命令就跑完，不经过模型。哪天 App 改版跑不通了，它会告诉你卡在哪一步、为什么。",
        "start": "三步上手：装到 Mac，接上 iPhone，让 agent 开工。",
        "outro": "iphone-use，开源，一行命令就能装。",
    },
    "en": {
        "hook": "Lots of apps have no API, so an AI can't tap them for you. iphone-use lets it use your real iPhone.",
        "how": "A daemon on your Mac drives WebDriverAgent on the phone over USB. The agent just calls an API.",
        "features": "It reads the screen as text and taps by element, says so when a tap didn't land, and tells you within seconds when the phone is locked.",
        "browser": "Remote control: watch the phone live in a browser or the iPhone app, and tap and type right there.",
        "wireframe": "Payment apps block screenshots, so the capture is blank. iphone-use draws their buttons and text from the accessibility tree.",
        "api": "One step: read the screen, tap by element, and get the settled change back in about four seconds.",
        "perf": "The speed is earned. The screen is read once, and a tap only re-checks its target. Frames tell when the screen has settled. What can be cached is cached, and every request is timed.",
        "perf-numbers": "Tap and settle went from eight and a half seconds to four point two. A cold connect, from seventy-five seconds to thirteen.",
        "tokens": "It saves tokens too: the screen comes as text, not a screenshot every step; you get only what changed; a tap brings back its result; and up to twenty-four steps go in one call.",
        "flows-loop": "Anything you do once doesn't need a model to rediscover it. Record it as a flow, get it reviewed into the official registry, and anyone can run it.",
        "flows": "Exporting your Health data is one command, with no model in the loop. And when an app update breaks a flow, it tells you which step failed, and why.",
        "start": "Three steps: install on the Mac, plug in the iPhone, and let the agent work.",
        "outro": "iphone-use. Open source, one command to install.",
    },
}

for lang, lines in SCRIPT.items():
    voice, rate = VOICES[lang]
    out = HERE / "voiceover" / lang
    out.mkdir(parents=True, exist_ok=True)
    for scene, text in lines.items():
        mp3 = out / f"{scene}.mp3"
        subprocess.run(
            ["edge-tts", "--voice", voice, f"--rate={rate}", "--text", text, "--write-media", str(mp3)],
            check=True, capture_output=True,
        )
        print(lang, scene, mp3.stat().st_size)
