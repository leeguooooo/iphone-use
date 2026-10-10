import SwiftUI
import UIKit

// The chrome around a phone's picture, shared by the live remote, sync and
// the demo: a slim top bar (or a side rail in landscape), the picture as
// large as the screen allows, a key bar, the live keyboard, and immersive
// mode that hides everything but the phone.

/// Lays out the remote: portrait stacks bar / picture / keys; landscape
/// puts the bar and the keys in rails beside the picture; immersive hides
/// both and gives the picture the whole screen.
struct RemoteScaffold<Top: View, Screen: View, Accessory: View, Banner: View, Keys: View, Keyboard: View>: View {
    var immersive: Bool
    var typing: Bool
    var onExitImmersive: () -> Void
    @ViewBuilder var top: (Axis) -> Top
    @ViewBuilder var screen: () -> Screen
    @ViewBuilder var accessory: (Axis) -> Accessory
    /// A one-line state (stalled, reconnecting, in use elsewhere): laid out
    /// above the picture, never over it.
    @ViewBuilder var banner: () -> Banner
    @ViewBuilder var keys: (Axis) -> Keys
    @ViewBuilder var keyboard: () -> Keyboard
    @Environment(\.verticalSizeClass) private var verticalSize

    private var landscape: Bool { verticalSize == .compact }

    var body: some View {
        Group {
            if landscape {
                HStack(spacing: 0) {
                    if !immersive {
                        top(.vertical)
                            .padding(.vertical, Theme.Space.s)
                            .transition(.move(edge: .leading).combined(with: .opacity))
                    }
                    accessory(.vertical)
                    VStack(spacing: 0) {
                        banner()
                        screen()
                            .padding(immersive ? 0 : Theme.Space.s)
                        if typing { keyboard() }
                    }
                    if !immersive && !typing {
                        keys(.vertical)
                            .padding(.vertical, Theme.Space.s)
                            .transition(.move(edge: .trailing).combined(with: .opacity))
                    }
                }
                .padding(.horizontal, immersive ? 0 : Theme.Space.s)
            } else {
                VStack(spacing: 0) {
                    if !immersive {
                        top(.horizontal)
                            .padding(.horizontal, Theme.Space.m)
                            .padding(.bottom, Theme.Space.s)
                            .transition(.move(edge: .top).combined(with: .opacity))
                    }
                    accessory(.horizontal)
                    banner()
                    screen()
                        .padding(.horizontal, immersive ? 0 : Theme.Space.s)
                        .padding(.vertical, immersive ? 0 : Theme.Space.xs)
                    if typing {
                        keyboard()
                    } else if !immersive {
                        keys(.horizontal)
                            .padding(.horizontal, Theme.Space.m)
                            .padding(.top, Theme.Space.s)
                            .padding(.bottom, Theme.Space.xs)
                            .transition(.move(edge: .bottom).combined(with: .opacity))
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.stage.ignoresSafeArea())
        // In the corner the hidden status bar leaves free (beside the Dynamic
        // Island; in landscape, in the side inset), never over the picture.
        .overlay(alignment: .topTrailing) {
            if immersive && !typing {
                GlassIconButton(symbol: "arrow.down.right.and.arrow.up.left", label: "退出沉浸模式", size: 32,
                                action: onExitImmersive)
                    .opacity(0.6)
                    .padding(.top, landscape ? 12 : 10)
                    .padding(.trailing, landscape ? 12 : 18)
                    .ignoresSafeArea()
                    .transition(.opacity)
            }
        }
        .statusBarHidden(immersive)
        .persistentSystemOverlays(immersive ? .hidden : .automatic)
        .animation(.snappy(duration: 0.3), value: immersive)
        .animation(.snappy(duration: 0.3), value: typing)
    }
}

// MARK: - Top bar

/// Which phone, how it is connected, and how fast: name, state or route,
/// round trip and frame rate. Tap it for the phone's settings.
struct DeviceBadge: View {
    let name: String
    let health: DeviceSession.Health
    /// The state when it is not simply ready, else the route.
    let caption: String
    var captionTone: Color = .secondary
    var rttMs: Int?
    var fps: Int?
    var axis: Axis = .horizontal

    var body: some View {
        Group {
            if axis == .horizontal {
                HStack(spacing: Theme.Space.s) {
                    HealthDot(health: health)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(name).font(.subheadline.weight(.semibold)).lineLimit(1)
                        HStack(spacing: 6) {
                            if !caption.isEmpty {
                                Text(caption).foregroundStyle(captionTone).lineLimit(1)
                            }
                            LatencyLabel(rttMs: rttMs, fps: fps)
                        }
                        .font(.caption2)
                    }
                }
                .padding(.leading, 12).padding(.trailing, 14).padding(.vertical, 6)
                .glassBackground(Capsule())
            } else {
                VStack(spacing: 4) {
                    HealthDot(health: health, size: 10)
                    LatencyLabel(rttMs: rttMs, fps: fps, stacked: true)
                        .font(.caption2)
                }
                .frame(width: 48)
                .padding(.vertical, 8)
                .glassBackground(RoundedRectangle(cornerRadius: 14, style: .continuous))
            }
        }
        .dynamicTypeSize(...DynamicTypeSize.accessibility1)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(accessibilityText))
        .accessibilityHint(Text("打开这台手机的设置"))
        .accessibilityAddTraits(.isButton)
    }

    private var accessibilityText: String {
        var parts = [name]
        if !caption.isEmpty { parts.append(caption) }
        if let rttMs { parts.append(String(localized: "延迟 \(rttMs) 毫秒")) }
        if let fps, fps > 0 { parts.append(String(localized: "每秒 \(fps) 帧")) }
        return parts.joined(separator: "，")
    }
}

/// "24 ms · 30 fps", colored by how the round trip feels.
struct LatencyLabel: View {
    let rttMs: Int?
    let fps: Int?
    var stacked = false

    var body: some View {
        if rttMs != nil || (fps ?? 0) > 0 {
            let layout = stacked ? AnyLayout(VStackLayout(spacing: 1)) : AnyLayout(HStackLayout(spacing: 4))
            layout {
                if let rttMs {
                    Text("\(rttMs) ms").foregroundStyle(color(rttMs))
                }
                if let fps, fps > 0 {
                    Text("\(fps) fps").foregroundStyle(.secondary)
                }
            }
            .monospacedDigit()
            .lineLimit(1)
        }
    }

    private func color(_ ms: Int) -> Color {
        ms < 60 ? Theme.ok : ms < 150 ? Theme.busy : Theme.down
    }
}

/// "2.5×": tap to zoom back out to fit.
struct ZoomChip: View {
    let scale: CGFloat
    let reset: () -> Void

    var body: some View {
        Button(action: reset) {
            HStack(spacing: 4) {
                Image(systemName: "arrow.down.right.and.arrow.up.left").imageScale(.small)
                Text(String(format: "%.1f×", scale)).monospacedDigit()
            }
            .font(.caption.weight(.semibold))
            .padding(.horizontal, 10).frame(height: 32)
            .glassBackground(Capsule())
        }
        .buttonStyle(PressableStyle())
        .foregroundStyle(.primary)
        .accessibilityLabel(Text("缩放 \(String(format: "%.1f", scale)) 倍，点按还原"))
        .accessibilityIdentifier("zoom-chip")
        .accessibilityValue(Text(String(format: "%.2f", scale)))
        .transition(.scale.combined(with: .opacity))
    }
}

/// The remote's top bar, or (vertical) its leading rail in landscape.
struct RemoteTopBar<Badge: View>: View {
    var axis: Axis
    var backSymbol = "square.grid.2x2"
    var backLabel: LocalizedStringKey = "全部手机"
    var zoom: CGFloat = 1
    var onBack: () -> Void
    var onResetZoom: () -> Void
    var onImmersive: () -> Void
    @ViewBuilder var badge: () -> Badge

    var body: some View {
        if axis == .horizontal {
            HStack(spacing: Theme.Space.s) {
                GlassIconButton(symbol: backSymbol, label: backLabel, size: 38, action: onBack)
                badge()
                Spacer(minLength: 0)
                if zoom > 1.01 { ZoomChip(scale: zoom, reset: onResetZoom) }
                GlassIconButton(symbol: "arrow.up.left.and.arrow.down.right", label: "沉浸模式", size: 38,
                                action: onImmersive)
            }
            .animation(.snappy, value: zoom > 1.01)
        } else {
            VStack(spacing: Theme.Space.m) {
                GlassIconButton(symbol: backSymbol, label: backLabel, size: 40, action: onBack)
                badge()
                Spacer(minLength: 0)
                if zoom > 1.01 { ZoomChip(scale: zoom, reset: onResetZoom) }
                GlassIconButton(symbol: "arrow.up.left.and.arrow.down.right", label: "沉浸模式", size: 40,
                                action: onImmersive)
            }
            .frame(width: 56)
            .animation(.snappy, value: zoom > 1.01)
        }
    }
}

// MARK: - Key bar

/// The phone's keys: back, Home, search, keyboard, and a menu for the rest.
/// Icons with short names in portrait; a rail of icons in landscape.
struct KeyBar<Menu: View>: View {
    var axis: Axis
    var onBack: () -> Void
    var onHome: () -> Void
    var onSearch: () -> Void
    var onKeyboard: () -> Void
    var keysEnabled = true
    @ViewBuilder var menu: () -> Menu

    var body: some View {
        let layout = axis == .horizontal ? AnyLayout(HStackLayout(spacing: 0)) : AnyLayout(VStackLayout(spacing: 2))
        layout {
            KeyButton(symbol: "chevron.backward", title: "返回", compact: axis == .vertical, action: onBack)
                .disabled(!keysEnabled)
            KeyButton(symbol: "house", title: "主屏幕", compact: axis == .vertical, action: onHome)
                .disabled(!keysEnabled)
            KeyButton(symbol: "magnifyingglass", title: "搜索", compact: axis == .vertical, action: onSearch)
                .disabled(!keysEnabled)
            KeyButton(symbol: "keyboard", title: "键盘", compact: axis == .vertical, action: onKeyboard)
                .disabled(!keysEnabled)
            SwiftUI.Menu {
                menu()
            } label: {
                KeyFace(symbol: "ellipsis", title: "更多", compact: axis == .vertical)
            }
            .menuOrder(.fixed)
            .tint(.primary)
            .accessibilityLabel(Text("更多"))
        }
        .padding(axis == .horizontal ? EdgeInsets(top: 4, leading: 6, bottom: 4, trailing: 6)
                                     : EdgeInsets(top: 6, leading: 4, bottom: 6, trailing: 4))
        .frame(maxWidth: axis == .horizontal ? 520 : 56)
        .glassBackground(RoundedRectangle(cornerRadius: axis == .horizontal ? 22 : 20, style: .continuous))
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
    }
}

struct KeyButton: View {
    let symbol: String
    let title: LocalizedStringKey
    var compact = false
    let action: () -> Void

    var body: some View {
        Button(action: {
            Haptics.light()
            action()
        }) {
            KeyFace(symbol: symbol, title: title, compact: compact)
        }
        .buttonStyle(PressableStyle())
        .accessibilityLabel(Text(title))
        .accessibilityIdentifier("key-" + symbol)
    }
}

struct KeyFace: View {
    let symbol: String
    let title: LocalizedStringKey
    var compact = false
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        VStack(spacing: 2) {
            Image(systemName: symbol)
                .font(.system(size: 18, weight: .medium))
                .frame(height: 24)
            if !compact {
                Text(title).font(.caption2).lineLimit(1).minimumScaleFactor(0.8)
            }
        }
        .frame(maxWidth: .infinity, minHeight: compact ? 44 : 46)
        .frame(width: compact ? 48 : nil)
        .contentShape(Rectangle())
        .foregroundStyle(isEnabled ? AnyShapeStyle(.primary) : AnyShapeStyle(.tertiary))
    }
}

// MARK: - Live keyboard

/// Typing goes to the phone as it happens: each committed character (after
/// the input method settles, so Pinyin is sent as the chosen characters),
/// each backspace and return. Sends run one after the other, in order.
@MainActor
@Observable
final class TypingQueue {
    enum Op: Equatable {
        case text(String)
        case key(String)
    }

    private(set) var pending = 0
    private(set) var sentCharacters = 0
    private var ops: [Op] = []
    private var worker: Task<Void, Never>?
    private let perform: (PhoneAction) async -> Void

    init(perform: @escaping (PhoneAction) async -> Void) {
        self.perform = perform
    }

    /// The field being typed into, so the bar's keys edit it too (and the
    /// phone keeps matching what the field shows).
    weak var editor: PhoneKeyboardField.Coordinator?

    func backspace() {
        if let editor { editor.backspace() } else { enqueue(.key("backspace")) }
    }

    func newline() {
        if let editor { editor.newline() } else { enqueue(.key("return")) }
    }

    func paste(_ text: String) {
        guard !text.isEmpty else { return }
        if let editor { editor.insert(text) } else { enqueue(.text(text)) }
    }

    func enqueue(_ op: Op) {
        // Consecutive text goes out as one message.
        if case let .text(new) = op, case let .text(old)? = ops.last {
            ops[ops.count - 1] = .text(old + new)
        } else {
            ops.append(op)
        }
        pending = ops.count
        guard worker == nil else { return }
        worker = Task { [weak self] in
            while let self, !self.ops.isEmpty {
                let op = self.ops.removeFirst()
                self.pending = self.ops.count + 1
                switch op {
                case let .text(text):
                    await self.perform(.text(text))
                    self.sentCharacters += text.count
                case let .key(name):
                    await self.perform(.key(name))
                }
                self.pending = self.ops.count
            }
            self?.worker = nil
        }
    }
}

/// The text field the system keyboard types into. What it holds is what
/// has been sent since the last return; edits become backspaces and text.
final class PhoneTextField: UITextField {
    /// Backspace with nothing left in the field still reaches the phone.
    var onDeleteWhenEmpty: (() -> Void)?

    override func deleteBackward() {
        if (text ?? "").isEmpty { onDeleteWhenEmpty?() }
        super.deleteBackward()
    }
}

struct PhoneKeyboardField: UIViewRepresentable {
    let queue: TypingQueue
    var placeholder: String

    func makeCoordinator() -> Coordinator {
        let coordinator = Coordinator(queue: queue)
        queue.editor = coordinator
        return coordinator
    }

    func makeUIView(context: Context) -> PhoneTextField {
        let field = PhoneTextField()
        field.placeholder = placeholder
        field.font = .preferredFont(forTextStyle: .body)
        field.adjustsFontForContentSizeCategory = true
        field.textColor = .label
        field.tintColor = Theme.uiAccent
        field.returnKeyType = .default
        field.autocorrectionType = .no
        field.spellCheckingType = .no
        field.smartQuotesType = .no
        field.smartDashesType = .no
        field.smartInsertDeleteType = .no
        field.keyboardAppearance = .dark
        field.delegate = context.coordinator
        context.coordinator.field = field
        field.accessibilityLabel = String(localized: "输入到手机")
        field.accessibilityHint = String(localized: "输入的文字会实时发到手机上")
        field.addTarget(context.coordinator, action: #selector(Coordinator.changed(_:)), for: .editingChanged)
        field.onDeleteWhenEmpty = { [weak queue] in queue?.enqueue(.key("backspace")) }
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        DispatchQueue.main.async { field.becomeFirstResponder() }
        return field
    }

    func updateUIView(_ uiView: PhoneTextField, context: Context) {}

    @MainActor
    final class Coordinator: NSObject, UITextFieldDelegate {
        let queue: TypingQueue
        weak var field: PhoneTextField?
        /// What the phone has received since the last return.
        private var sent = ""

        init(queue: TypingQueue) { self.queue = queue }

        func backspace() {
            guard let field, !(field.text ?? "").isEmpty else {
                queue.enqueue(.key("backspace"))
                return
            }
            field.deleteBackward()
            changed(field)
        }

        func newline() {
            guard let field else { return queue.enqueue(.key("return")) }
            _ = textFieldShouldReturn(field)
        }

        func insert(_ text: String) {
            guard let field else { return queue.enqueue(.text(text)) }
            field.insertText(text)
            changed(field)
        }

        @objc func changed(_ field: UITextField) {
            // Wait for the input method to commit (Pinyin, Japanese, …).
            guard field.markedTextRange == nil else { return }
            let text = field.text ?? ""
            let edit = TypingDiff.edit(from: sent, to: text)
            for _ in 0..<edit.backspaces { queue.enqueue(.key("backspace")) }
            if !edit.insert.isEmpty { queue.enqueue(.text(edit.insert)) }
            sent = text
        }

        func textFieldShouldReturn(_ field: UITextField) -> Bool {
            queue.enqueue(.key("return"))
            field.text = ""
            sent = ""
            return false
        }
    }
}

/// How to turn what the phone has into what the field now holds, typing
/// only at the end (the phone's cursor): backspace past the common prefix,
/// then type the rest.
enum TypingDiff {
    static func edit(from old: String, to new: String) -> (backspaces: Int, insert: String) {
        let a = Array(old)
        let b = Array(new)
        var common = 0
        while common < a.count, common < b.count, a[common] == b[common] { common += 1 }
        return (a.count - common, String(b[common...]))
    }
}

/// The bar above the system keyboard while typing into the phone.
struct LiveKeyboardBar: View {
    let queue: TypingQueue
    /// Sync: how many phones receive it.
    var targets = 1
    let onDone: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Image(systemName: "dot.radiowaves.left.and.right").imageScale(.small)
                Text(targets > 1 ? "实时输入到 \(targets) 台手机" : "实时输入到手机：先在画面上点一下输入框")
                    .lineLimit(1)
                Spacer(minLength: 0)
                if queue.pending > 0 {
                    ProgressView().controlSize(.mini)
                    Text("发送中").monospacedDigit()
                } else if queue.sentCharacters > 0 {
                    Text("已发送 \(queue.sentCharacters) 字").monospacedDigit()
                }
            }
            .font(.caption)
            .foregroundStyle(targets > 1 ? AnyShapeStyle(Theme.attention) : AnyShapeStyle(.secondary))
            HStack(spacing: Theme.Space.s) {
                GlassIconButton(symbol: "keyboard.chevron.compact.down", label: "收起键盘", size: 38, action: onDone)
                PhoneKeyboardField(queue: queue, placeholder: String(localized: "输入的文字实时发到手机"))
                    .frame(height: 38)
                    .padding(.horizontal, 14)
                    .glassBackground(Capsule())
                PasteButton(payloadType: String.self) { strings in
                    let text = strings.joined(separator: "\n")
                    Task { @MainActor in queue.paste(text) }
                }
                .labelStyle(.iconOnly)
                .buttonBorderShape(.circle)
                .tint(Color(white: 0.25))
                .accessibilityLabel(Text("把剪贴板粘贴到手机"))
                GlassIconButton(symbol: "delete.left", label: "退格", size: 38) { queue.backspace() }
                GlassIconButton(symbol: "return", label: "换行", size: 38) { queue.newline() }
            }
        }
        .padding(.horizontal, Theme.Space.m)
        .padding(.vertical, Theme.Space.s)
        .background(.bar)
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        .transition(.move(edge: .bottom).combined(with: .opacity))
    }
}

// MARK: - Status over the picture

/// What covers the picture (or floats above it) when the phone cannot simply
/// be driven: one title, one explanation, one obvious button, a spinner
/// with the seconds waited, and when the next automatic try is.
struct StatusOverlay: View {
    let app: AppModel
    let session: DeviceSession

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let p = session.presentation(now: context.date)
            if p.placement == .cover {
                StatusCard(p: p, busy: session.busy) { perform($0, on: session, app: app) }
            }
        }
        .animation(.snappy(duration: 0.25), value: session.presentation().placement)
    }
}

/// The strip form of the state, for the scaffold's banner slot: above the
/// picture, so it never hides the phone's status bar or its top buttons.
struct StatusBanner: View {
    let app: AppModel
    let session: DeviceSession

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let p = session.presentation(now: context.date)
            if p.placement == .banner {
                StatusStrip(p: p) { perform($0, on: session, app: app) }
            }
        }
        .animation(.snappy(duration: 0.25), value: session.presentation().placement == .banner)
    }
}

struct StatusCard: View {
    let p: ConnectionPresentation
    var busy = false
    let run: (ConnectionPresentation.Action) -> Void

    var body: some View {
        VStack(spacing: Theme.Space.m) {
            ZStack {
                Circle().fill(Theme.color(p.tone).opacity(0.16)).frame(width: 56, height: 56)
                if p.progress {
                    ProgressView().controlSize(.regular).tint(Theme.color(p.tone))
                } else {
                    Image(systemName: p.symbol).font(.title2.weight(.semibold)).foregroundStyle(Theme.color(p.tone))
                }
            }
            .accessibilityHidden(true)
            Text(p.title).font(.headline).multilineTextAlignment(.center)
                .accessibilityAddTraits(.isHeader)
            if !p.detail.isEmpty {
                Text(p.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if p.elapsed != nil || p.retryIn != nil {
                HStack(spacing: Theme.Space.m) {
                    if let elapsed = p.elapsed { Text("已等待 \(elapsed) 秒") }
                    if let retryIn = p.retryIn { Text("\(retryIn) 秒后自动重试") }
                }
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
            }
            if p.primary != nil || p.secondary != nil {
                VStack(spacing: Theme.Space.s) {
                    if let primary = p.primary {
                        Button(ConnectionPresentation.label(primary)) { run(primary) }
                            .buttonStyle(ProminentButtonStyle())
                            .disabled(busy)
                    }
                    if let secondary = p.secondary {
                        Button(ConnectionPresentation.label(secondary)) { run(secondary) }
                            .font(.subheadline.weight(.semibold))
                            .frame(minHeight: 36)
                    }
                }
                .padding(.top, Theme.Space.xs)
            }
        }
        .padding(Theme.Space.xl)
        .frame(maxWidth: 340)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Theme.Radius.card, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.card, style: .continuous).strokeBorder(Theme.hairline))
        .shadow(color: .black.opacity(0.35), radius: 24, y: 8)
        .padding()
        .accessibilityElement(children: .contain)
        .transition(.scale(scale: 0.96).combined(with: .opacity))
    }
}

/// A one-line state over the top of the picture: it can still be driven.
struct StatusStrip: View {
    let p: ConnectionPresentation
    let run: (ConnectionPresentation.Action) -> Void

    var body: some View {
        HStack(spacing: Theme.Space.s) {
            if p.progress {
                ProgressView().controlSize(.mini).tint(Theme.color(p.tone))
            } else {
                Image(systemName: p.symbol).foregroundStyle(Theme.color(p.tone)).accessibilityHidden(true)
            }
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(p.title).font(.footnote.weight(.semibold))
                    if let elapsed = p.elapsed {
                        Text("· \(elapsed) 秒").font(.footnote.monospacedDigit()).foregroundStyle(.secondary)
                    }
                }
                if !p.detail.isEmpty {
                    Text(p.detail).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                }
            }
            if let primary = p.primary {
                Button(ConnectionPresentation.label(primary)) { run(primary) }
                    .font(.caption.weight(.semibold))
                    .buttonStyle(.bordered)
                    .buttonBorderShape(.capsule)
                    .controlSize(.small)
            }
        }
        .padding(.leading, 12).padding(.trailing, p.primary == nil ? 14 : 6).padding(.vertical, 7)
        .glassBackground(Capsule())
        .padding(.horizontal)
        .padding(.bottom, Theme.Space.xs)
        .accessibilityElement(children: .combine)
        .transition(.move(edge: .top).combined(with: .opacity))
    }
}

struct ToastLayer: View {
    let text: String?

    var body: some View {
        VStack {
            Spacer()
            if let text {
                Text(text)
                    .font(.callout)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 16).padding(.vertical, 10)
                    .glassBackground(Capsule())
                    .shadow(color: .black.opacity(0.3), radius: 12, y: 4)
                    .padding(.bottom, Theme.Space.m)
                    .padding(.horizontal)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
                    .accessibilityAddTraits(.updatesFrequently)
            }
        }
        .allowsHitTesting(false)
        .animation(.snappy(duration: 0.25), value: text)
    }
}
