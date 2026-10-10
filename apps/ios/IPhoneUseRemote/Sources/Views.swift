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
    @State private var manualAfterScan = false

    var body: some View {
        Group {
            if let demo = app.demo {
                DemoView(session: demo) { app.demo = nil }
                    .transition(.opacity)
            } else if app.devices.isEmpty {
                WelcomeView(app: app)
                    .transition(.opacity)
            } else if let session = app.focused {
                Group {
                    if let group = app.sync, group.lead == session.id {
                        SyncView(app: app, lead: session, group: group)
                    } else {
                        RemoteView(app: app, session: session)
                    }
                }
                // A different phone is a different screen (its own zoom,
                // keyboard queue).
                .id(session.id)
                .transition(.asymmetric(insertion: .scale(scale: 0.94).combined(with: .opacity),
                                        removal: .opacity))
            } else {
                DeviceGridView(app: app)
                    .transition(.asymmetric(insertion: .opacity.combined(with: .scale(scale: 1.03)),
                                            removal: .opacity))
            }
        }
        .animation(.snappy(duration: 0.32), value: app.focusedID)
        .animation(.snappy(duration: 0.32), value: app.demo == nil)
        .tint(Theme.accent)
        .onAppear {
            #if DEBUG
            // Screenshots: `-orientation landscape`.
            if UserDefaults.standard.string(forKey: "orientation") == "landscape",
               let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene {
                scene.requestGeometryUpdate(.iOS(interfaceOrientations: .landscapeRight)) { _ in }
            }
            #endif
        }
        // A device's "enter password" and "scan again" buttons, from any screen.
        .sheet(item: $app.editing) { target in
            NavigationStack {
                DeviceEditForm(app: app, id: target.id, closesSheet: true)
            }
        }
        .fullScreenCover(isPresented: $app.scanning, onDismiss: {
            // "Enter by hand" from the scanner: once the camera is gone.
            if manualAfterScan, let id = app.focusedID {
                manualAfterScan = false
                app.editing = .init(id: id)
            }
        }) {
            ScanSheet(onFound: { link in Task { await app.pair(link) } },
                      onManual: app.focusedID == nil ? nil : { manualAfterScan = true })
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
            Label("这台设备没有联网，联网后会自动重连", systemImage: "wifi.slash")
                .font(.footnote.weight(.semibold))
                .padding(.horizontal, 14).padding(.vertical, 8)
                .foregroundStyle(Theme.attention)
                .background(Theme.attention.opacity(0.16), in: Capsule())
                .padding(.horizontal)
                .padding(.bottom, Theme.Space.s)
                .accessibilityAddTraits(.isStaticText)
                .transition(.move(edge: .top).combined(with: .opacity))
        }
    }
}

// MARK: - Connect

/// Pair a phone by address and password (pushed from the welcome screen),
/// or the "add device" sheet with its scan button. `onDone` (sheet only)
/// closes it once the device is saved.
struct ConnectView: View {
    @Bindable var app: AppModel
    let onDone: (() -> Void)?
    /// Pushed inside the welcome screen's navigation, without its own.
    var embedded = false
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
        if embedded {
            form
        } else {
            NavigationStack { form }
        }
    }

    private var form: some View {
        Form {
            if !embedded {
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
                    Text("每台手机在 Mac 上各有一个 iphone-use 页面（不同端口），各扫一次它自己的二维码。")
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
                Text(embedded ? "Mac 地址和控制密码" : "或者手动输入")
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
        .navigationTitle(embedded ? "手动连接" : "添加手机")
        .navigationBarTitleDisplayMode(.inline)
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
    @State private var scanning = false
    @Environment(\.dismiss) private var dismiss

    /// What it is called with the name left empty: the phone's own name.
    private var placeholderName: String {
        app.devices.first { $0.id == id }?.phone?.label ?? DeviceStore.defaultName(for: address)
    }

    var body: some View {
        let session = app.session(id)
        Form {
            if let session {
                Section {
                    ConnectionSummary(session: session)
                }
            }
            if let readiness = session?.status?.lockReadiness {
                LockReadinessSection(readiness: readiness)
            }
            Section("名称") {
                TextField(placeholderName, text: $name)
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
                    scanning = true
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
            name = record.customName ?? ""
            address = record.address
        }
        .onChange(of: address) { problem = nil }
        .onChange(of: password) { problem = nil }
        // Presented from here: this form may itself sit in a sheet, where the
        // root's scanner cannot appear.
        .fullScreenCover(isPresented: $scanning) {
            ScanSheet(onFound: { link in
                Task {
                    if case .connected = await app.pair(link) { dismiss() }
                }
            })
        }
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

/// Passcode and Auto-Lock for one phone, with the daemon's advice. Read-only:
/// the setting is changed on the phone itself (Settings › Display & Brightness).
struct LockReadinessSection: View {
    let readiness: LockReadiness

    var body: some View {
        Section {
            LabeledContent("锁屏密码", value: readiness.passcodeText)
            LabeledContent("自动锁定", value: readiness.autoLockText ?? String(localized: "未知"))
            if let badge = readiness.badge {
                LockBadgeView(badge: badge, compact: false)
            }
        } header: {
            Text("锁屏设置")
        } footer: {
            if !readiness.hint.isEmpty {
                Text(readiness.hint)
            }
        }
    }
}


// MARK: - Remote

/// One phone full screen.
struct RemoteView: View {
    @Bindable var app: AppModel
    let session: DeviceSession
    @State private var typing = false
    @State private var showSettings = false
    @State private var immersive = false
    @State private var zoom: CGFloat = 1
    @State private var resetZoom = 0
    @State private var queue: TypingQueue

    init(app: AppModel, session: DeviceSession) {
        self.app = app
        self.session = session
        _queue = State(initialValue: TypingQueue { [weak session] action in await session?.perform(action) })
    }

    var body: some View {
        RemoteScaffold(immersive: immersive, typing: typing, onExitImmersive: { immersive = false },
                       toast: session.toast ?? app.toast) { axis in
            RemoteTopBar(axis: axis, zoom: zoom,
                         onBack: { app.focusedID = nil },
                         onResetZoom: { resetZoom += 1 },
                         onImmersive: { immersive = true }) {
                Button { showSettings = true } label: { SessionBadge(session: session, axis: axis) }
                    .buttonStyle(PressableStyle())
                    .foregroundStyle(.primary)
            }
        } screen: {
            RemoteStage(app: app, session: session, zoom: $zoom, resetZoom: resetZoom) { session.send($0) }
        } accessory: { _ in
            OfflineBanner(online: app.online)
        } banner: {
            StatusBanner(app: app, session: session)
        } keys: { axis in
            KeyBar(axis: axis,
                   onBack: { session.send(.back) },
                   onHome: { session.send(.home) },
                   onSearch: { session.send(.spotlight) },
                   onKeyboard: { typing = true },
                   keysEnabled: session.phase == .connected) {
                RemoteMenu(app: app, session: session, immersive: $immersive, showSettings: $showSettings)
            }
        } keyboard: {
            LiveKeyboardBar(queue: queue, onDone: { typing = false })
        }
        .sheet(isPresented: $showSettings) { SettingsSheet(app: app, session: session) }
        .onAppear {
            #if DEBUG
            // Screenshots: `-immersive YES`, `-typing YES`, `-settings YES`.
            let defaults = UserDefaults.standard
            if defaults.bool(forKey: "immersive") { immersive = true }
            if defaults.bool(forKey: "typing") { typing = true }
            if defaults.bool(forKey: "settings") { showSettings = true }
            // `-toast <text>` holds a toast up.
            if let toast = defaults.string(forKey: "toast") { session.toast = toast }
            #endif
        }
    }
}

/// The picture of one phone with everything drawn over it: the touch
/// surface, a wireframe where the app hides its screen from capture, and
/// the state card or strip. (Toasts go in the chrome, off the picture.)
struct RemoteStage: View {
    let app: AppModel
    let session: DeviceSession
    @Binding var zoom: CGFloat
    var resetZoom: Int
    let onAction: (PhoneAction) -> Void
    @Environment(\.pictureFrameSink) private var pictureFrameSink

    var body: some View {
        ZStack {
            TimelineView(.periodic(from: .now, by: 1)) { context in
                let p = session.presentation(now: context.date)
                RemoteScreen(session: session, options: CanvasOptions(
                    dimmed: p.dimsPicture,
                    resetZoom: resetZoom,
                    onZoom: { zoom = $0 },
                    onPictureFrame: { pictureFrameSink($0) },
                    overlay: session.redactedImage,
                    accessibilityLabel: String(localized: "\(session.name) 的屏幕"),
                    accessibilityActions: [
                        (String(localized: "主屏幕"), { session.send(.home) }),
                        (String(localized: "返回"), { session.send(.back) }),
                        (String(localized: "搜索"), { session.send(.spotlight) }),
                    ]), onAction: onAction)
            }
            StatusOverlay(app: app, session: session)
        }
    }
}

/// The phone's name, its state (or route when all is well), round trip and
/// frame rate, for the top bar.
struct SessionBadge: View {
    let session: DeviceSession
    var axis: Axis = .horizontal

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let p = session.presentation(now: context.date)
            let ready = p.tone == .ok
            DeviceBadge(name: session.name, health: p.tone,
                        caption: ready ? (session.routeLabel ?? "") : p.short,
                        captionTone: ready ? .secondary : Theme.color(p.tone),
                        rttMs: session.phase == .connected ? session.rttMs : nil,
                        fps: ready ? session.fps : nil,
                        axis: axis)
        }
    }
}

/// The key bar's "more" menu: picture quality, immersive mode, hand back
/// or reconnect the phone, settings.
struct RemoteMenu: View {
    @Bindable var app: AppModel
    let session: DeviceSession
    @Binding var immersive: Bool
    @Binding var showSettings: Bool

    var body: some View {
        Picker(selection: $app.videoPreference) {
            ForEach(VideoPreference.allCases, id: \.self) { choice in
                Label(choice.title, systemImage: choice.symbol).tag(choice)
            }
        } label: {
            Label("画面", systemImage: "sparkles.tv")
        }
        .pickerStyle(.menu)
        Button { immersive = true } label: {
            Label("沉浸模式", systemImage: "arrow.up.left.and.arrow.down.right")
        }
        if session.status?.humanHandoff == true || session.status?.released == true {
            Button { session.connectPhone() } label: { Label("连接手机", systemImage: "bolt.horizontal") }
                .disabled(session.busy)
        } else {
            Button { session.handBack() } label: { Label("交还手机", systemImage: "iphone.and.arrow.forward") }
                .disabled(session.phase != .connected)
        }
        Divider()
        Button { showSettings = true } label: { Label("设置", systemImage: "gearshape") }
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
                    HStack(spacing: Theme.Space.m) {
                        Image(systemName: "iphone")
                            .font(.title2)
                            .foregroundStyle(Theme.accent)
                            .frame(width: 48, height: 48)
                            .background(Theme.accent.opacity(0.15), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                            .accessibilityHidden(true)
                        VStack(alignment: .leading, spacing: 3) {
                            Text(session.name).font(.headline)
                            if let phone = session.record.phone {
                                Text([phone.model, phone.ios.map { "iOS \($0)" }].compactMap { $0 }.joined(separator: " · "))
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            ConnectionSummary(session: session)
                        }
                    }
                    .padding(.vertical, 4)
                }
                Section("名称") {
                    TextField(session.record.defaultDisplayName, text: $name)
                        .onSubmit { app.rename(session.id, to: name) }
                        .submitLabel(.done)
                }
                Section {
                    Picker("画面", selection: $app.videoPreference) {
                        ForEach(VideoPreference.allCases, id: \.self) { choice in
                            VStack(alignment: .leading, spacing: 2) {
                                Text(choice.title)
                                Text(choice.detail).font(.caption).foregroundStyle(.secondary)
                            }
                            .tag(choice)
                        }
                    }
                    .pickerStyle(.inline)
                    .labelsHidden()
                } header: {
                    Text("画面")
                } footer: {
                    Text(videoFooter)
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
                    if let rtt = session.rttMs {
                        LabeledContent("这台设备到 Mac", value: "\(rtt) ms")
                    }
                    if let link = session.status?.transportLabel {
                        let rtt = session.status?.phoneRttMs.map { " · \($0) ms" } ?? ""
                        LabeledContent("Mac 到手机", value: link + rtt)
                    }
                    LabeledContent("服务版本", value: session.status?.version ?? "—")
                }
                if let readiness = session.status?.lockReadiness {
                    LockReadinessSection(readiness: readiness)
                }
                Section {
                    if session.status?.humanHandoff == true || session.status?.released == true {
                        Button { session.connectPhone(); dismiss() } label: {
                            Label("连接手机", systemImage: "bolt.horizontal")
                        }
                    } else if session.phase == .connected {
                        Button { session.handBack(); dismiss() } label: {
                            Label("交还手机", systemImage: "iphone.and.arrow.forward")
                        }
                    }
                    if session.phase == .connected {
                        Button { session.disconnect(); dismiss() } label: {
                            Label("断开", systemImage: "xmark.circle")
                        }
                    } else {
                        Button { Task { await session.connect(password: nil) }; dismiss() } label: {
                            Label("立即重试", systemImage: "arrow.clockwise")
                        }
                    }
                } footer: {
                    Text("交还：停止远程控制，手机留给拿着它的人用。断开：这台设备不再连接这台 Mac。")
                }
                Section {
                    Button(role: .destructive) { confirmForget = true } label: {
                        Label("忘记这台手机", systemImage: "trash")
                    }
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
            .onAppear { name = session.record.customName ?? "" }
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

    /// What auto comes to now, and how fast the last picture started.
    private var videoFooter: String {
        let now = session.preferQuality ? VideoPreference.quality.title : VideoPreference.performance.title
        var text = String(localized: "现在是「\(now)」。格子里的小画面总是用流畅。")
        if let ms = session.firstFrameMs {
            text += " " + String(localized: "上次画面 \(ms) 毫秒出现。")
        }
        return text
    }
}
