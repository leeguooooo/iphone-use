import Foundation

/// The language the app's own text is shown in, so text the daemon sends in
/// both languages (`next_step`, lock-screen hints) matches the rest of the UI.
enum AppLanguage {
    /// The localization iOS picked for this app from the person's preferred
    /// languages: Chinese (zh-Hans), or English for everything else.
    static var isChinese: Bool {
        Bundle.main.preferredLocalizations.first?.hasPrefix("zh") ?? false
    }

    /// One `{zh, en}` pair in the app's language, else whichever half exists.
    static func pick(_ pair: [String: String]?, chinese: Bool = isChinese) -> String {
        let zh = pair?["zh"].flatMap { $0.isEmpty ? nil : $0 }
        let en = pair?["en"].flatMap { $0.isEmpty ? nil : $0 }
        return (chinese ? zh ?? en : en ?? zh) ?? ""
    }
}
