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
                    Text("\(link.base.host() ?? link.base.absoluteString)\n只在你刚刚扫了自己 Mac 上的二维码时才点「连接」。")
                }
        }
    }
}

struct RootView: View {
    @Bindable var app: AppModel

    var body: some View {
        if let demo = app.demo {
            DemoView(session: demo) { app.demo = nil }
        } else if app.devices.isEmpty {
            if app.adding {
                ProgressView("正在连接…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(Color.black)
            } else {
                ConnectView(app: app, onDone: nil)
            }
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
}

// MARK: - Connect

/// Pair a phone: the first-launch screen, and the "add device" sheet.
/// `onDone` (sheet only) closes it once the new device is saved.
struct ConnectView: View {
    @Bindable var app: AppModel
    let onDone: (() -> Void)?
    @State private var address = UserDefaults.standard.string(forKey: DeviceStore.legacyAddressKey) ?? ""
    @State private var password = ""
    @State private var scanning = false
    @Environment(\.dismiss) private var dismiss

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
                if let why = app.addError {
                    Section {
                        Label(why, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                }
                Section {
                    TextField("http://192.168.1.11:44321", text: $address)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField("控制密码", text: $password)
                } header: {
                    Text("或者手动输入")
                } footer: {
                    Text("地址和密码在 Mac 上运行安装程序时会打印出来。手机和 Mac 需要在同一个网络里，或者通过 VPN 连通。")
                }
                Section {
                    Button {
                        Task {
                            await app.add(address: address, password: password)
                            if app.addError == nil { onDone?() }
                        }
                    } label: {
                        if app.adding {
                            ProgressView().frame(maxWidth: .infinity)
                        } else {
                            Text("用密码连接").frame(maxWidth: .infinity)
                        }
                    }
                    .disabled(address.isEmpty || password.isEmpty || app.adding)
                }
            }
            .navigationTitle(onDone == nil ? "Phone Use Remote" : "添加手机")
            .navigationBarTitleDisplayMode(onDone == nil ? .large : .inline)
            .toolbar {
                if onDone != nil {
                    ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
                }
            }
            .fullScreenCover(isPresented: $scanning) {
                ScanSheet { link in
                    Task {
                        await app.pair(link)
                        if app.addError == nil { onDone?() }
                    }
                }
            }
            .onAppear { app.addError = nil }
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
                if let overlay = StatusOverlay.content(for: session) {
                    StatusOverlay(content: overlay, session: session)
                }
                ToastLayer(text: session.toast)
            }
            Toolbar(app: app, session: session, typing: $typing, showSettings: $showSettings)
        }
        .background(Color.black.ignoresSafeArea())
        .animation(.easeInOut(duration: 0.2), value: session.toast)
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

struct StatusOverlay: View {
    struct Content {
        enum Action { case connect, reconnect }
        let title: String
        let detail: String
        let action: Action?
    }
    let content: Content
    let session: DeviceSession

    /// What covers the picture, if anything: why it cannot be driven now.
    static func content(for session: DeviceSession) -> Content? {
        switch session.phase {
        case .connecting:
            return .init(title: String(localized: "正在连接…"), detail: "", action: nil)
        case .setup:
            return .init(title: String(localized: "未连接"), detail: session.address, action: .reconnect)
        case let .failed(why):
            return .init(title: String(localized: "连接失败"), detail: why, action: .reconnect)
        case .connected:
            break
        }
        guard let status = session.status else { return nil }
        if status.humanHandoff {
            return .init(title: String(localized: "手机已交还"), detail: String(localized: "手机在持有人手里，远程控制已停止。需要远程操作时点「连接手机」。"), action: .connect)
        }
        if status.releasing {
            return .init(title: String(localized: "正在释放设备"), detail: String(localized: "请稍候"), action: nil)
        }
        if status.setupBlockedOn == "locked" {
            return .init(title: String(localized: "请解锁手机"), detail: String(localized: "手机锁着屏，设备服务启动不了。解锁并保持亮屏后会自动接着连接。"), action: nil)
        }
        if status.reconnecting {
            let blocked = !status.setupBlockedOn.isEmpty
            return .init(title: blocked ? String(localized: "需要处理一下") : String(localized: "正在连接手机"),
                         detail: blocked ? status.personHint : String(localized: "手机锁着的话请解锁一次；第一次连接可能要一两分钟。"),
                         action: nil)
        }
        if status.released {
            return .init(title: String(localized: "设备空闲中"), detail: String(localized: "一段时间没人操作，设备连接已暂停。点「连接手机」继续（手机需解锁亮屏）。"), action: .connect)
        }
        if status.deviceState == "locked" || status.locked == true {
            return .init(title: String(localized: "手机锁屏了"), detail: String(localized: "锁屏密码界面不能远程输入，请在手机上解锁。远程操作时可以把「自动锁定」调长一点。"), action: nil)
        }
        if status.deviceState == "offline" || status.deviceState == "blocked" {
            return .init(title: String(localized: "连不上手机"), detail: status.personHint.isEmpty ? String(localized: "设备服务没有运行") : status.personHint,
                         action: status.recoveryOwner == "daemon" ? .connect : nil)
        }
        if !session.videoLive {
            return .init(title: String(localized: "正在加载画面…"), detail: session.videoMessage ?? "", action: nil)
        }
        return nil
    }

    var body: some View {
        VStack(spacing: 12) {
            Text(content.title).font(.headline)
            if !content.detail.isEmpty {
                Text(content.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            switch content.action {
            case .connect:
                Button("连接手机") { session.connectPhone() }
                    .buttonStyle(.borderedProminent)
                    .disabled(session.busy)
            case .reconnect:
                Button("重新连接") { Task { await session.connect(password: nil) } }
                    .buttonStyle(.borderedProminent)
            case nil:
                EmptyView()
            }
        }
        .padding(24)
        .frame(maxWidth: 340)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 20))
        .padding()
    }
}

struct HealthDot: View {
    let health: DeviceSession.Health

    var body: some View {
        Circle().fill(color).frame(width: 8, height: 8)
    }

    private var color: Color {
        switch health {
        case .ok: return .green
        case .busy: return .yellow
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
        HStack {
            ToolButton(title: "主屏幕", symbol: "house") { session.send(.home) }
            ToolButton(title: "键盘", symbol: "keyboard") { typing = true }
            if handedOver || session.status?.released == true {
                ToolButton(title: "连接手机", symbol: "bolt.horizontal") { session.connectPhone() }
            } else {
                ToolButton(title: "交还", symbol: "iphone.and.arrow.forward") { session.handBack() }
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
        .foregroundStyle(.primary)
    }
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
                Section("名称") {
                    TextField(DeviceStore.defaultName(for: session.address), text: $name)
                        .onSubmit { app.rename(session.id, to: name) }
                        .submitLabel(.done)
                }
                Section("连接") {
                    LabeledContent("服务地址", value: session.address)
                    LabeledContent("服务版本", value: session.status?.version ?? "—")
                    LabeledContent("设备状态", value: session.status?.deviceState ?? "—")
                }
                Section {
                    if session.phase == .connected {
                        Button("断开") { session.disconnect(); dismiss() }
                    } else {
                        Button("重新连接") { Task { await session.connect(password: nil) }; dismiss() }
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
