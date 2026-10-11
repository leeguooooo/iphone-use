import Foundation

// Which models a task may use, chosen by the person once for every device:
// an ordered list of "assist" chat models (the first that answers wins), and
// optionally Jev (TypeSafe) with its text model. Sent with each task as
// `models` (docs/agent-task.md); the daemon uses them for that task only.
// Keys live in the Keychain (`model:<id>`, `model:jev`, `model:text`), never
// in UserDefaults or logs; the list itself is not secret.

/// One assist model the person set up.
struct ModelProvider: Codable, Equatable, Identifiable, Sendable {
    enum Kind: String, Codable, CaseIterable, Sendable {
        /// Kilo Gateway's `:free` models: no key at all.
        case free
        /// Zen's public free-tier credential is not a personal API key.
        case zen
        case openRouter
        case anthropic
        /// Any OpenAI-compatible (or Anthropic-format) endpoint.
        case custom
    }

    enum Format: String, Codable, CaseIterable, Sendable {
        case openai, anthropic
    }

    var id: UUID
    var kind: Kind
    var name: String
    var format: Format
    var baseURL: String
    var model: String
    /// Off keeps it in the list without sending it.
    var enabled: Bool

    init(id: UUID = UUID(), kind: Kind, name: String? = nil, format: Format? = nil,
         baseURL: String? = nil, model: String? = nil, enabled: Bool = true) {
        self.id = id
        self.kind = kind
        self.name = name ?? kind.title
        self.format = format ?? kind.defaultFormat
        self.baseURL = baseURL ?? kind.defaultBaseURL
        self.model = model ?? kind.defaultModel
        self.enabled = enabled
    }

    /// The Keychain account of its key.
    var keyAccount: String { "model:" + id.uuidString }
    /// The free tier is sent without a key; every other one needs one.
    var needsKey: Bool { kind != .free && kind != .zen }
    var isFree: Bool { kind == .free || kind == .zen }
    func requestKey(_ secret: String?) -> String {
        if kind == .zen { return "public" }
        return needsKey ? (secret ?? "").trimmingCharacters(in: .whitespacesAndNewlines) : ""
    }
    /// The person may change where it points (presets keep theirs).
    var editableEndpoint: Bool { kind == .custom }
}

extension ModelProvider.Kind {
    var title: String {
        switch self {
        case .free: return String(localized: "免费试用")
        case .zen: return "OpenCode Zen · Free"
        case .openRouter: return "OpenRouter"
        case .anthropic: return "Anthropic"
        case .custom: return String(localized: "自定义（OpenAI 兼容）")
        }
    }

    var detail: String {
        switch self {
        case .free: return String(localized: "Kilo Gateway 的免费模型，不用密钥；有频率限制")
        case .zen: return String(localized: "Zen 免费模型，public 已自动配置，无需个人密钥；有频率限制")
        case .openRouter: return String(localized: "用你自己的 OpenRouter 密钥；:free 模型不扣费")
        case .anthropic: return String(localized: "用你自己的 Anthropic 密钥")
        case .custom: return String(localized: "任何兼容 OpenAI 接口的服务：地址、密钥、模型")
        }
    }

    var defaultFormat: ModelProvider.Format { self == .anthropic ? .anthropic : .openai }

    var defaultBaseURL: String {
        switch self {
        case .free: return "https://api.kilo.ai/api/openrouter"
        case .zen: return "https://opencode.ai/zen/v1"
        case .openRouter: return "https://openrouter.ai/api/v1"
        case .anthropic: return "https://api.anthropic.com"
        case .custom: return ""
        }
    }

    /// Free models come and go and hit shared daily limits, so `free` lets
    /// the daemon pick from the gateway's current free list (models that
    /// read screenshots first) and move on when one does not answer. The
    /// person can type a specific `…:free` id instead.
    var defaultModel: String {
        switch self {
        case .free, .openRouter: return "free"
        case .zen: return "big-pickle"
        case .anthropic: return "claude-sonnet-5-5"
        case .custom: return ""
        }
    }
}

/// Everything the person chose, without the keys.
struct ModelSettings: Codable, Equatable, Sendable {
    /// Off: send no `models`, so the daemon uses its own (a Mac's environment).
    var sendModels = true
    /// Tried in this order; the first that answers is used.
    var assist: [ModelProvider] = [ModelProvider(kind: .zen), ModelProvider(kind: .free)]
    var jevModel = "jev-latest"
    var textBaseURL = "https://openrouter.ai/api/v1"
    var textModel = "inception/mercury-2.5"

    static let jevBaseURL = "https://api.typesafe.ai/v1"
    static let jevKeyAccount = "model:jev"
    static let textKeyAccount = "model:text"

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let fresh = ModelSettings()
        sendModels = try c.decodeIfPresent(Bool.self, forKey: .sendModels) ?? fresh.sendModels
        assist = try c.decodeIfPresent([ModelProvider].self, forKey: .assist) ?? fresh.assist
        jevModel = try c.decodeIfPresent(String.self, forKey: .jevModel) ?? fresh.jevModel
        textBaseURL = try c.decodeIfPresent(String.self, forKey: .textBaseURL) ?? fresh.textBaseURL
        textModel = try c.decodeIfPresent(String.self, forKey: .textModel) ?? fresh.textModel
    }

    /// The `models` object for a task, with keys looked up by account; nil
    /// when nothing usable is set (or sending is off), so the daemon falls
    /// back to its own configuration.
    ///
    /// An assist entry is sent when it is on and has an address, a model and
    /// (unless it is the free tier) a key. Jev goes only with its key; the
    /// text model only with a key, its own or, for OpenRouter, that of an
    /// OpenRouter entry in the list.
    func payload(key: (String) -> String?) -> ModelsPayload? {
        guard sendModels else { return nil }
        func secret(_ account: String) -> String {
            (key(account) ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        }
        let assist: [ModelsPayload.Assist] = self.assist.compactMap { provider in
            let base = provider.baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
            let model = provider.model.trimmingCharacters(in: .whitespacesAndNewlines)
            let key = provider.requestKey(provider.needsKey ? secret(provider.keyAccount) : nil)
            guard provider.enabled, !base.isEmpty, !model.isEmpty, !provider.needsKey || !key.isEmpty else {
                return nil
            }
            return .init(format: provider.format.rawValue, baseURL: base, key: key, model: model)
        }
        var jev: ModelsPayload.Endpoint?
        let jevKey = secret(Self.jevKeyAccount)
        if !jevKey.isEmpty {
            let model = jevModel.trimmingCharacters(in: .whitespacesAndNewlines)
            jev = .init(baseURL: Self.jevBaseURL, key: jevKey, model: model.isEmpty ? "jev-latest" : model)
        }
        var text: ModelsPayload.Endpoint?
        let textBase = textBaseURL.trimmingCharacters(in: .whitespacesAndNewlines)
        let textModel = textModel.trimmingCharacters(in: .whitespacesAndNewlines)
        var textKey = secret(Self.textKeyAccount)
        if textKey.isEmpty, textBase == ModelProvider.Kind.openRouter.defaultBaseURL {
            textKey = self.assist.first { $0.kind == .openRouter && !secret($0.keyAccount).isEmpty }
                .map { secret($0.keyAccount) } ?? ""
        }
        if !textKey.isEmpty, !textBase.isEmpty, !textModel.isEmpty {
            text = .init(baseURL: textBase, key: textKey, model: textModel)
        }
        if jev == nil, text == nil, assist.isEmpty { return nil }
        return ModelsPayload(jev: jev, text: text, assist: assist)
    }
}

/// `models` as the daemon reads it (docs/agent-task.md › Models).
struct ModelsPayload: Encodable, Equatable, Sendable {
    struct Endpoint: Encodable, Equatable, Sendable {
        var baseURL: String
        var key: String
        var model: String

        enum CodingKeys: String, CodingKey {
            case baseURL = "base_url"
            case key, model
        }
    }

    struct Assist: Encodable, Equatable, Sendable {
        var format: String
        var baseURL: String
        /// Empty (but present) for the free tier.
        var key: String
        var model: String

        enum CodingKeys: String, CodingKey {
            case format
            case baseURL = "base_url"
            case key, model
        }
    }

    var jev: Endpoint?
    var text: Endpoint?
    var assist: [Assist]

    enum CodingKeys: String, CodingKey { case jev, text, assist }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encodeIfPresent(jev, forKey: .jev)
        try c.encodeIfPresent(text, forKey: .text)
        if !assist.isEmpty { try c.encode(assist, forKey: .assist) }
    }
}

/// Where the settings are kept: the list in UserDefaults, keys in the Keychain.
enum ModelSettingsStore {
    static let key = "agent.models.v1"
    private static let zenPresetKey = "agent.models.zen-preset.v1"

    /// The saved settings; the first time, the defaults, saved at once so
    /// the free tier keeps one id (and a key added to it later stays put).
    static func load(from defaults: UserDefaults) -> ModelSettings {
        guard let data = defaults.data(forKey: key),
              var settings = try? JSONDecoder().decode(ModelSettings.self, from: data) else {
            let fresh = ModelSettings()
            save(fresh, to: defaults)
            defaults.set(true, forKey: zenPresetKey)
            return fresh
        }
        if !defaults.bool(forKey: zenPresetKey) {
            // Upgrade only the untouched free preset. Never replace a person's
            // chosen providers, keys, IDs, order, or enabled state.
            if settings.assist.count == 1, let first = settings.assist.first,
               first.kind == .free, first.baseURL == ModelProvider.Kind.free.defaultBaseURL,
               first.model == "free", first.enabled {
                settings.assist.insert(ModelProvider(kind: .zen), at: 0)
                save(settings, to: defaults)
            }
            defaults.set(true, forKey: zenPresetKey)
        }
        return settings
    }

    static func save(_ settings: ModelSettings, to defaults: UserDefaults) {
        if let data = try? JSONEncoder().encode(settings) {
            defaults.set(data, forKey: key)
        }
    }

    /// A key from the Keychain; nil when none is saved.
    static func secret(_ account: String) -> String? {
        Keychain.password(for: account)
    }

    /// Save a key, or (empty) delete it.
    static func setSecret(_ value: String, for account: String) {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            Keychain.delete(for: account)
        } else {
            Keychain.save(password: trimmed, for: account)
        }
    }
}
