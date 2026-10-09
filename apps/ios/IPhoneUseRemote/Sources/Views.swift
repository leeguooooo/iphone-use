import SwiftUI

@main
struct IPhoneUseRemoteApp: App {
    @State private var app = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView(app: app)
                .preferredColorScheme(.dark)
                // The landing page a scanned QR opens hands off here.
                .onOpenURL { app.handle(url: $0) }
                .onChange(of: scenePhase) { _, phase in
                    switch phase {
                    case .active: app.becameActive()
                    case .background: app.enteredBackground()
                    default: break
                    }
                }
                .alert(
                    "连接到这台 Mac？",
                    isPresented: Binding(
                        get: { app.pendingLink != nil },
                        set: { if !$0 { app.pendingLink = nil } }),
                    presenting: app.pendingLink
                ) { _ in
                    Button("连接") { app.confirmPendingLink() }
                    Button("取消", role: .cancel) { app.pendingLink = nil }
                } message: { link in
                    if let name = app.savedName(for: link) {
                        Text("\(link.base.host() ?? link.base.absoluteString)\n已保存为「\(name)」，会刷新它的配对。只在你刚刚扫了自己 Mac 上的二维码时才点「连接」。")
                    } else {
                        Text("\(link.base.host() ?? link.base.absoluteString)\n只在你刚刚扫了自己 Mac 上的二维码时才点「连接」。")
                    }
                }
        }
    }
}

struct RootView: View {
    @Bindable var app: AppModel

    var body: some View {
        Group {
            if let demo = app.demo {
                DemoView(session: demo) { app.demo = nil }
            } else if app.devices.isEmpty {
                ConnectView(app: app, onDone: nil)
            } else if let session = app.focused {
                if let group = app.sync, group.lead == session.id {
                    SyncView(app: app, lead: session, group: group)
                } else {
                    RemoteView(app: app, session: session)
                }
            } else {
                DeviceGridView(app: app)
            }
        }
        // A device's "enter password" and "scan again" buttons, from any screen.
        .sheet(item: $app.editing) { target in
            NavigationStack {
                DeviceEditForm(app: app, id: target.id, closesSheet: true)
            }
        }
        .fullScreenCover(isPresented: $app.scanning) {
            ScanSheet(onFound: { link in Task { await app.pair(link) } })
        }
    }
}

/// Run a state's button.
@MainActor
func perform(_ action: ConnectionPresentation.Action, on session: DeviceSession, app: AppModel) {
    switch action {
    case .retry: Task { await session.connect(password: nil) }
    case .login: app.editing = .init(id: session.id)
    case .rescan: app.scanning = true
    case .wakePhone: session.connectPhone()
    case .reloadVideo: session.reloadVideo()
    case .openSettings:
        if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
    }
}

/// "No network" strip, shown above everything while this iPhone is offline.
struct OfflineBanner: View {
    let online: Bool

    var body: some View {
        if !online {
            Label("这台 iPhone 没有联网，联网后会自动重连", systemImage: "wifi.slash")
                .font(.footnote.weight(.semibold))
                .padding(.horizontal, 12).padding(.vertical, 6)
                .frame(maxWidth: .infinity)
                .background(Color.orange.opacity(0.85), in: RoundedRectangle(cornerRadius: 10))
                .foregroundStyle(.black)
                .padding(.horizontal)
                .accessibilityAddTraits(.isStaticText)
        }
    }
}

// MARK: - Connect

/// Pair a phone: the first-launch screen, and the "add device" sheet.
/// `onDone` (sheet only) closes it once the device is saved.
struct ConnectView: View {
    @Bindable var app: AppModel
    let onDone: (() -> Void)?
    @State private var address = UserDefaults.standard.string(forKey: DeviceStore.legacyAddressKey) ?? ""
    @State private var password = ""
    @State private var scanning = false
    @State private var working: Task<Void, Never>?
    @State private var problem: String?
    @State private var canSaveAnyway = false
    @FocusState private var field: Field?
    @Environment(\.dismiss) private var dismiss

    enum Field { case address, password }

    private var input: AddressInput? { AddressInput(address) }
    private var isPairLink: Bool { if case .pair = input { return true } else { return false } }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Button {
                        scanning = true
                    } label: {
                        Label("扫码连接", systemImage: "qrcode.viewfinder")
                            .font(.headline)
                            .frame(maxWidth: .infinity, minHeight: 44)
                    }
                    .buttonStyle(.borderedProminent)
                    .listRowInsets(EdgeInsets())
                    .listRowBackground(Color.clear)
                    .disabled(app.adding)
                } footer: {
                    Text(onDone == nil
                         ? "在 Mac 上打开 iphone-use 页面，点工具栏的「扫码」，再用这里扫一下。用 iPhone 自带相机扫也可以。"
                         : "每台手机在 Mac 上各有一个 iphone-use 页面（不同端口），各扫一次它自己的二维码。")
                }
                if onDone == nil {
                    Section {
                        Button {
                            app.startDemo()
                        } label: {
                            Label("试用演示", systemImage: "play.rectangle")
                                .frame(maxWidth: .infinity, minHeight: 44)
                        }
                        .buttonStyle(.bordered)
                        .listRowInsets(EdgeInsets())
                        .listRowBackground(Color.clear)
                    } footer: {
                        Text("还没装好 Mac 端？先看看：播放一段从真实 iPhone 录下的画面，可以点几下体验操作方式。不连接任何设备。")
                    }
                }
                Section {
                    TextField("192.168.1.11:44321", text: $address)
                        .keyboardType(.URL)
                        .textContentType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($field, equals: .address)
                        .submitLabel(.next)
                        .onSubmit { field = .password }
                        .accessibilityLabel(Text("Mac 地址"))
                    if !isPairLink {
                        SecureField("控制密码", text: $password)
                            .textContentType(.password)
                            .focused($field, equals: .password)
                            .submitLabel(.go)
                            .onSubmit(submit)
                    }
                } header: {
                    Text("或者手动输入")
                } footer: {
                    AddressHint(text: address)
                }
                if let problem = problem ?? app.addError {
                    Section {
                        Label(problem, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                        if canSaveAnyway {
                            Button("仍然保存这台 Mac") { submit(saveAnyway: true) }
                        }
                    }
                }
                Section {
                    Button(action: { submit() }) {
                        if app.adding {
                            HStack(spacing: 10) {
                                ProgressView()
                                Text("正在连接…")
                            }
                            .frame(maxWidth: .infinity)
                        } else {
                            Text(isPairLink ? "用这个配对链接连接" : "连接并保存").frame(maxWidth: .infinity)
                        }
                    }
                    .disabled(!canSubmit)
                    if app.adding {
                        Button("取消", role: .cancel) { working?.cancel() }
                            .frame(maxWidth: .infinity)
                    }
                } footer: {
                    Text("地址和密码在 Mac 上运行安装程序时会打印出来（也可以在 Mac 上运行 iphone-use status 查看）。现在连不上也会先保存，Mac 醒来或回到同一个网络后会自动连接。")
                }
            }
            .navigationTitle(onDone == nil ? "Phone Use Remote" : "添加手机")
            .navigationBarTitleDisplayMode(onDone == nil ? .large : .inline)
            .toolbar {
                if onDone != nil {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("取消") { working?.cancel(); dismiss() }
                    }
                }
            }
            .fullScreenCover(isPresented: $scanning) {
                ScanSheet(
                    onFound: { link in
                        working = Task {
                            await app.pair(link)
                            onDone?()
                        }
                    },
                    onManual: { field = .address })
            }
            .onAppear { app.addError = nil }
            .onChange(of: address) { problem = nil; canSaveAnyway = false }
            .onChange(of: password) { if !canSaveAnyway { problem = nil } }
        }
    }

    private var canSubmit: Bool {
        guard !app.adding, input != nil else { return false }
        return isPairLink || !password.isEmpty
    }

    private func submit() { submit(saveAnyway: false) }

    private func submit(saveAnyway: Bool) {
        guard saveAnyway || canSubmit else { return }
        problem = nil
        canSaveAnyway = false
        field = nil
        working = Task {
            let outcome = await app.add(address: address, password: password, saveAnyway: saveAnyway)
            switch outcome {
            case .connected, .saved:
                onDone?()
            case let .rejected(message, saveable):
                problem = message
                canSaveAnyway = saveable
            }
        }
    }
}

/// Under the address field: what the typed text will connect to, so a
/// mistyped port or a pasted link is obvious before connecting.
struct AddressHint: View {
    let text: String

    var body: some View {
        if text.trimmingCharacters(in: .whitespaces).isEmpty {
            Text("可以填 IP（192.168.1.11）、IP:端口、http(s):// 地址或隧道域名；也可以粘贴二维码里的配对链接。")
        } else {
            switch AddressInput(text) {
            case let .address(url):
                Text("将连接 \(url.absoluteString)")
            case let .pair(link):
                Text("配对链接：会直接和 \(link.base.absoluteString) 配对，不需要密码")
            case nil:
                Text("地址格式不对，应该像 192.168.1.11:44321").foregroundStyle(.orange)
            }
        }
    }
}

/// Edit a saved device: name, address, password; also where a login prompt
/// lands. Saving with an unreachable Mac keeps the change and retries.
struct DeviceEditForm: View {
    @Bindable var app: AppModel
    let id: UUID
    /// Shown as its own sheet (a login prompt) rather than pushed in Settings.
    var closesSheet = false
    @State private var name = ""
    @State private var address = ""
    @State private var password = ""
    @State private var problem: String?
    @State private var loaded = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        let session = app.session(id)
        Form {
            if let session {
                Section {
                    ConnectionSummary(session: session)
                }
            }
            Section("名称") {
                TextField(DeviceStore.defaultName(for: address), text: $name)
            }
            Section {
                TextField("192.168.1.11:44321", text: $address)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .accessibilityLabel(Text("Mac 地址"))
                SecureField(hasPassword ? "留空则保留已保存的密码" : "控制密码", text: $password)
                    .textContentType(.password)
            } header: {
                Text("地址和密码")
            } footer: {
                AddressHint(text: address)
            }
            if let problem {
                Section {
                    Label(problem, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                }
            }
            Section {
                Button(action: save) {
                    if app.adding {
                        HStack(spacing: 10) { ProgressView(); Text("正在连接…") }.frame(maxWidth: .infinity)
                    } else {
                        Text("保存并连接").frame(maxWidth: .infinity)
                    }
                }
                .disabled(app.adding || DaemonClient.parse(address: address) == nil)
                Button {
                    if closesSheet { dismiss() }
                    app.scanning = true
                } label: {
                    Label("在 Mac 上重新扫码", systemImage: "qrcode.viewfinder")
                }
            } footer: {
                Text("改了地址也会保留配对和局域网线路。连不上时会先保存，之后自动重试。")
            }
        }
        .navigationTitle(closesSheet ? String(localized: "登录") : String(localized: "编辑"))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if closesSheet {
                ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
            }
        }
        .onAppear {
            guard !loaded, let record = app.devices.first(where: { $0.id == id }) else { return }
            loaded = true
            name = record.name
            address = record.address
        }
        .onChange(of: address) { problem = nil }
        .onChange(of: password) { problem = nil }
    }

    private var hasPassword: Bool {
        app.devices.first { $0.id == id }.map { Keychain.password(for: $0.address) != nil } ?? false
    }

    private func save() {
        problem = nil
        Task {
            let outcome = await app.update(id, name: name, address: address, password: password)
            switch outcome {
            case .connected:
                dismiss()
            case let .saved(_, why):
                if why.needsLogin {
                    problem = why.sentence
                } else {
                    app.show(String(localized: "已保存。现在连不上：\(why.title)，会自动重试"))
                    dismiss()
                }
            case let .rejected(message, _):
                problem = message
            }
        }
    }
}

/// One line on a device's state, for lists and forms.
struct ConnectionSummary: View {
    let session: DeviceSession

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let p = session.presentation(now: context.date)
            HStack(spacing: 10) {
                HealthDot(health: p.tone)
                VStack(alignment: .leading, spacing: 2) {
                    Text(p.title).font(.subheadline.weight(.semibold))
                    if !p.detail.isEmpty {
                        Text(p.detail).font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            .accessibilityElement(children: .combine)
        }
    }
}

// MARK: - Remote

/// One phone full screen: today's single-device view.
struct RemoteView: View {
    @Bindable var app: AppModel
    let session: DeviceSession
    @State private var typing = false
    @State private var showSettings = false

    var body: some View {
        // Status, picture and toolbar stacked so none of them covers the
        // phone's own status bar or home indicator.
        VStack(spacing: 6) {
            HStack(spacing: 8) {
                BackToGridButton(app: app)
                StatusPill(session: session, showName: app.devices.count > 1)
            }
            .padding(.horizontal)
            OfflineBanner(online: app.online)
            ZStack {
                RemoteScreen(session: session) { session.send($0) }
                if let wireframe = session.redactedImage {
                    // The app hides this screen from capture; this is its
                    // accessibility tree drawn by the daemon. Taps pass through.
                    Image(uiImage: wireframe)
                        .resizable()
                        .scaledToFit()
                        .allowsHitTesting(false)
                }
                StatusOverlay(app: app, session: session)
                ToastLayer(text: session.toast ?? app.toast)
            }
            Toolbar(app: app, session: session, typing: $typing, showSettings: $showSettings)
        }
        .background(Color.black.ignoresSafeArea())
        .animation(.easeInOut(duration: 0.2), value: session.toast)
        .animation(.easeInOut(duration: 0.2), value: app.toast)
        .sheet(isPresented: $typing) {
            TypeSheet(targets: 1) { session.send(.text($0)) }
        }
        .sheet(isPresented: $showSettings) { SettingsSheet(app: app, session: session) }
    }
}

/// Back to the overview of every phone.
struct BackToGridButton: View {
    let app: AppModel
    var action: (() -> Void)?

    var body: some View {
        Button {
            if let action { action() } else { app.focusedID = nil }
        } label: {
            Image(systemName: "square.grid.2x2")
                .font(.system(size: 17, weight: .semibold))
                .frame(width: 36, height: 32)
                .background(.ultraThinMaterial, in: Capsule())
        }
        .foregroundStyle(.primary)
        .accessibilityLabel(Text("全部手机"))
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
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(.bottom, 10)
                    .padding(.horizontal)
                    .transition(.opacity)
            }
        }
        .allowsHitTesting(false)
    }
}

/// What covers the picture (or sits above it) when the phone cannot simply
/// be driven: one title, one explanation, one obvious button, a spinner
/// with the seconds waited, and when the next automatic try is.
struct StatusOverlay: View {
    let app: AppModel
    let session: DeviceSession

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let p = session.presentation(now: context.date)
            switch p.placement {
            case .cover: card(p)
            case .banner:
                VStack {
                    banner(p)
                    Spacer()
                }
            case .none: EmptyView()
            }
        }
    }

    private func card(_ p: ConnectionPresentation) -> some View {
        VStack(spacing: 12) {
            if p.progress {
                ProgressView().controlSize(.large)
            } else {
                Image(systemName: p.symbol).font(.largeTitle).foregroundStyle(.secondary)
                    .accessibilityHidden(true)
            }
            Text(p.title).font(.headline).multilineTextAlignment(.center)
                .accessibilityAddTraits(.isHeader)
            if !p.detail.isEmpty {
                Text(p.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let elapsed = p.elapsed {
                Text("已等待 \(elapsed) 秒").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            if let retryIn = p.retryIn {
                Text("\(retryIn) 秒后自动重试").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            if let primary = p.primary {
                Button(ConnectionPresentation.label(primary)) { perform(primary, on: session, app: app) }
                    .buttonStyle(.borderedProminent)
                    .disabled(session.busy)
            }
            if let secondary = p.secondary {
                Button(ConnectionPresentation.label(secondary)) { perform(secondary, on: session, app: app) }
                    .buttonStyle(.bordered)
            }
        }
        .padding(24)
        .frame(maxWidth: 360)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 20))
        .padding()
        .accessibilityElement(children: .contain)
    }

    private func banner(_ p: ConnectionPresentation) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: p.symbol).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(p.title).font(.footnote.weight(.semibold))
                if !p.detail.isEmpty {
                    Text(p.detail).font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(10)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
        .padding(8)
        .allowsHitTesting(false)
        .accessibilityElement(children: .combine)
    }
}

struct HealthDot: View {
    let health: DeviceSession.Health

    var body: some View {
        Circle().fill(color).frame(width: 8, height: 8)
            .accessibilityHidden(true)
    }

    private var color: Color {
        switch health {
        case .ok: return .green
        case .busy: return .yellow
        case .attention: return .orange
        case .down: return .red
        }
    }
}

struct StatusPill: View {
    let session: DeviceSession
    var showName = false

    var body: some View {
        HStack(spacing: 6) {
            HealthDot(health: session.health)
            Text(label).font(.caption.monospaced()).lineLimit(1)
            if let note = session.delivery {
                DeliveryBadge(outcome: note.outcome)
            }
        }
        .padding(.horizontal, 10).padding(.vertical, 5)
        .background(.ultraThinMaterial, in: Capsule())
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var label: String {
        var parts = [session.shortState]
        if showName { parts.insert(session.name, at: 0) }
        if let route = session.routeLabel { parts.append(route) }
        return parts.joined(separator: " · ")
    }
}

struct Toolbar: View {
    @Bindable var app: AppModel
    let session: DeviceSession
    @Binding var typing: Bool
    @Binding var showSettings: Bool

    var body: some View {
        let handedOver = session.status?.humanHandoff == true
        let connected = session.phase == .connected
        HStack {
            ToolButton(title: "主屏幕", symbol: "house") { session.send(.home) }
                .disabled(!connected)
            ToolButton(title: "键盘", symbol: "keyboard") { typing = true }
                .disabled(!connected)
            if handedOver || session.status?.released == true {
                ToolButton(title: "连接手机", symbol: "bolt.horizontal") { session.connectPhone() }
                    .disabled(session.busy)
            } else {
                ToolButton(title: "交还", symbol: "iphone.and.arrow.forward") { session.handBack() }
                    .disabled(!connected)
            }
            ToolButton(title: app.videoQuality ? "画质" : "性能",
                       symbol: app.videoQuality ? "sparkles.tv" : "bolt.horizontal.circle") {
                app.videoQuality.toggle()
            }
            ToolButton(title: "设置", symbol: "gearshape") { showSettings = true }
        }
        .padding(.horizontal, 8).padding(.vertical, 6)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 18))
        .padding(.horizontal)
        .padding(.bottom, 6)
    }
}

struct ToolButton: View {
    let title: LocalizedStringKey
    let symbol: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 3) {
                Image(systemName: symbol).font(.system(size: 20))
                Text(title).font(.caption2)
            }
            .frame(maxWidth: .infinity, minHeight: 44)
        }
        .foregroundStyle(isEnabled ? .primary : .tertiary)
    }

    @Environment(\.isEnabled) private var isEnabled
}

/// Type text into the phone; in sync, into every member (`targets` > 1).
struct TypeSheet: View {
    let targets: Int
    let send: (String) -> Void
    @State private var text = ""
    @FocusState private var focused: Bool
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                Text("先在手机画面上点一下输入框，再在这里输入；支持中文。")
                    .font(.footnote).foregroundStyle(.secondary)
                if targets > 1 {
                    Label("同步中：会输入到全部 \(targets) 台手机", systemImage: "rectangle.on.rectangle")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(.orange)
                }
                TextField("要输入的文字", text: $text, axis: .vertical)
                    .textFieldStyle(.roundedBorder)
                    .focused($focused)
                    .lineLimit(1...5)
                    .submitLabel(.send)
                    .onSubmit(submit)
                Spacer()
            }
            .padding()
            .navigationTitle("输入文字")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("关闭") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("发送", action: submit).disabled(text.isEmpty)
                }
            }
            .onAppear { focused = true }
        }
        .presentationDetents([.height(targets > 1 ? 300 : 260)])
    }

    private func submit() {
        guard !text.isEmpty else { return }
        send(text)
        text = ""
    }
}

struct SettingsSheet: View {
    @Bindable var app: AppModel
    let session: DeviceSession
    @State private var name = ""
    @State private var confirmForget = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section {
                    ConnectionSummary(session: session)
                }
                Section("名称") {
                    TextField(DeviceStore.defaultName(for: session.address), text: $name)
                        .onSubmit { app.rename(session.id, to: name) }
                        .submitLabel(.done)
                }
                Section("连接") {
                    NavigationLink {
                        DeviceEditForm(app: app, id: session.id)
                    } label: {
                        LabeledContent("服务地址", value: session.address)
                    }
                    if let route = session.routeLabel {
                        LabeledContent("线路", value: route)
                    }
                    LabeledContent("服务版本", value: session.status?.version ?? "—")
                    LabeledContent("设备状态", value: session.shortState)
                }
                Section {
                    if session.phase == .connected {
                        Button("断开") { session.disconnect(); dismiss() }
                    } else {
                        Button("立即重试") { Task { await session.connect(password: nil) }; dismiss() }
                    }
                    Button("忘记这台手机", role: .destructive) { confirmForget = true }
                }
            }
            .navigationTitle("设置")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("完成") {
                        app.rename(session.id, to: name)
                        dismiss()
                    }
                }
            }
            .onAppear { name = session.name }
            .confirmationDialog("忘记这台手机？", isPresented: $confirmForget, titleVisibility: .visible) {
                Button("忘记", role: .destructive) {
                    app.remove(session.id)
                    dismiss()
                }
            } message: {
                Text("会删掉这台手机的配对，之后要重新扫码才能连接。")
            }
        }
    }
}
