import Foundation

/// The task's observed state. A future action is only named when the daemon
/// has actually selected it; text being typed is never shown here.
struct AgentProgress: Equatable, Sendable {
    var current: String
    var next: String
    var steps: Int
    var terminal: Bool
    var succeeded: Bool

    init(current: String, next: String, steps: Int, terminal: Bool = false, succeeded: Bool = false) {
        self.current = current; self.next = next; self.steps = steps; self.terminal = terminal; self.succeeded = succeeded
    }

    init(task: AgentTaskSnapshot) {
        steps = task.steps.count
        terminal = task.state.isTerminal
        succeeded = task.state == .done
        if terminal {
            current = task.state.title
            next = String(localized: "查看任务结果")
        } else if let confirm = task.pendingConfirm {
            current = String(localized: "等你确认")
            next = String(localized: "批准后：") + Self.action(confirm.action, target: confirm.target)
        } else if let plan = task.plannedAction, task.phase == "acting" {
            current = Self.action(plan.action, target: plan.target)
            next = String(localized: "读取新页面")
        } else {
            switch task.phase {
            case "paused": current = String(localized: "已暂停"); next = String(localized: "继续后重新读取页面")
            case "flow": current = String(localized: "查找或执行流程"); next = String(localized: "检查流程结果")
            case "summary": current = String(localized: "核验任务结果"); next = String(localized: "返回任务结果")
            case "observing": current = String(localized: "读取当前页面"); next = String(localized: "选择下一步")
            case "jev", "assist": current = String(localized: "分析当前页面"); next = String(localized: "选择下一步")
            default: current = String(localized: "准备开始"); next = String(localized: "读取当前页面")
            }
        }
    }

    static func action(_ operation: String, target: String?) -> String {
        let displayTarget: String?
        if operation == "LAUNCH", let target, target.contains("."), !target.contains(" ") {
            switch target {
            case "com.apple.Preferences": displayTarget = String(localized: "设置")
            case "com.apple.Health": displayTarget = String(localized: "健康")
            case "com.apple.mobilesafari": displayTarget = "Safari"
            case "com.apple.MobileSMS": displayTarget = String(localized: "信息")
            case "com.tencent.xin": displayTarget = String(localized: "微信")
            default: displayTarget = nil
            }
        } else { displayTarget = target }
        let label = displayTarget.map { "「\(String($0.prefix(36)))」" } ?? ""
        switch operation {
        case "CLICK": return String(localized: "点击") + label
        case "TYPE_TEXT": return String(localized: "填写文本") + label
        case "LAUNCH": return String(localized: "打开应用") + label
        case "RUN_FLOW": return String(localized: "执行流程") + label
        case "SCROLL_UP", "SCROLL_DOWN": return String(localized: "滚动页面")
        case "BACK": return String(localized: "返回上一页")
        case "HOME": return String(localized: "回到主屏幕")
        case "SPOTLIGHT": return String(localized: "打开系统搜索")
        case "PRESS_RETURN": return String(localized: "按回车")
        case "WAIT": return String(localized: "等待页面")
        case "DONE": return String(localized: "核验任务结果")
        default: return String(localized: "执行下一步")
        }
    }
}
