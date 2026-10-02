import SwiftUI

@main
struct IPhoneUseRemoteApp: App {
    @State private var model = RemoteModel()

    var body: some Scene {
        WindowGroup {
            RootView(model: model)
                .preferredColorScheme(.dark)
                // The landing page a scanned QR opens hands off here.
                .onOpenURL { model.handle(url: $0) }
                .alert(
                    "连接到这台 Mac？",
                    isPresented: Binding(
                        get: { model.pendingLink != nil },
                        set: { if !$0 { model.pendingLink = nil } }),
                    presenting: model.pendingLink
                ) { _ in
                    Button("连接") { model.confirmPendingLink() }
                    Button("取消", role: .cancel) { model.pendingLink = nil }
                } message: { link in
                    Text("\(link.base.host() ?? link.base.absoluteString)\n只在你刚刚扫了自己 Mac 上的二维码时才点「连接」。")
                }
        }
    }
}

struct RootView: View {
    @Bindable var model: RemoteModel

    var body: some View {
        switch model.phase {
        case .connected:
            RemoteView(model: model)
        case .connecting:
            ProgressView("正在连接…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black)
        case .setup, .failed:
            ConnectView(model: model)
        }
    }
}

// MARK: - Connect

struct ConnectView: View {
    @Bindable var model: RemoteModel
    @State private var password = ""
    @State private var scanning = false
    @FocusState private var focused: Bool

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
                } footer: {
                    Text("在 Mac 上打开 iphone-use 页面，点工具栏的「扫码」，再用这里扫一下。用 iPhone 自带相机扫也可以。")
                }
                if case let .failed(why) = model.phase {
                    Section {
                        Label(why, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                }
                Section {
                    TextField("http://192.168.1.11:44321", text: $model.address)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($focused)
                    SecureField("控制密码", text: $password)
                } header: {
                    Text("或者手动输入")
                } footer: {
                    Text("地址和密码在 Mac 上运行安装程序时会打印出来。手机和 Mac 需要在同一个网络里，或者通过 VPN 连通。")
                }
                Section {
                    Button {
                        Task { await model.connect(password: password) }
                    } label: {
                        Text("用密码连接").frame(maxWidth: .infinity)
                    }
                    .disabled(model.address.isEmpty || password.isEmpty)
                }
            }
            .navigationTitle("iPhone Use")
            .fullScreenCover(isPresented: $scanning) { ScanSheet(model: model) }
        }
    }
}

// MARK: - Remote

struct RemoteView: View {
    @Bindable var model: RemoteModel
    @State private var typing = false
    @State private var showSettings = false

    var body: some View {
        // Status, picture and toolbar stacked so none of them covers the
        // phone's own status bar or home indicator.
        VStack(spacing: 6) {
            StatusPill(model: model)
            ZStack {
                RemoteScreen(model: model)
                if let overlay = overlay {
                    StatusOverlay(content: overlay, model: model)
                }
                VStack {
                    Spacer()
                    if let toast = model.toast {
                        Text(toast)
                            .font(.callout)
                            .padding(.horizontal, 14).padding(.vertical, 8)
                            .background(.ultraThinMaterial, in: Capsule())
                            .padding(.bottom, 10)
                            .transition(.opacity)
                    }
                }
            }
            Toolbar(model: model, typing: $typing, showSettings: $showSettings)
        }
        .background(Color.black.ignoresSafeArea())
        .animation(.easeInOut(duration: 0.2), value: model.toast)
        .sheet(isPresented: $typing) { TypeSheet(model: model) }
        .sheet(isPresented: $showSettings) { SettingsSheet(model: model) }
    }

    private var overlay: StatusOverlay.Content? {
        guard let status = model.status else { return nil }
        if status.humanHandoff {
            return .init(title: "手机已交还", detail: "手机在持有人手里，远程控制已停止。需要远程操作时点「连接手机」。", action: .connect)
        }
        if status.releasing {
            return .init(title: "正在释放设备", detail: "请稍候", action: nil)
        }
        if status.reconnecting {
            let blocked = !status.setupBlockedOn.isEmpty
            return .init(title: blocked ? "需要处理一下" : "正在连接手机",
                         detail: blocked ? status.hint : "手机锁着的话请解锁一次；第一次连接可能要一两分钟。",
                         action: nil)
        }
        if status.released {
            return .init(title: "设备空闲中", detail: "一段时间没人操作，设备连接已暂停。点「连接手机」继续（手机需解锁亮屏）。", action: .connect)
        }
        if status.deviceState == "locked" || status.locked == true {
            return .init(title: "手机锁屏了", detail: "锁屏密码界面不能远程输入，请在手机上解锁。远程操作时可以把「自动锁定」调长一点。", action: nil)
        }
        if status.deviceState == "offline" || status.deviceState == "blocked" {
            return .init(title: "连不上手机", detail: status.hint.isEmpty ? "WDA 没有运行" : status.hint,
                         action: status.recoveryOwner == "daemon" ? .connect : nil)
        }
        if !model.videoLive {
            return .init(title: "正在加载画面…", detail: model.videoMessage ?? "", action: nil)
        }
        return nil
    }
}

struct StatusOverlay: View {
    struct Content {
        enum Action { case connect }
        let title: String
        let detail: String
        let action: Action?
    }
    let content: Content
    let model: RemoteModel

    var body: some View {
        VStack(spacing: 12) {
            Text(content.title).font(.headline)
            if !content.detail.isEmpty {
                Text(content.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            if content.action == .connect {
                Button("连接手机") { model.connectPhone() }
                    .buttonStyle(.borderedProminent)
                    .disabled(model.busy)
            }
        }
        .padding(24)
        .frame(maxWidth: 340)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 20))
        .padding()
    }
}

struct StatusPill: View {
    let model: RemoteModel

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(color).frame(width: 8, height: 8)
            Text(label).font(.caption.monospaced())
        }
        .padding(.horizontal, 10).padding(.vertical, 5)
        .background(.ultraThinMaterial, in: Capsule())
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal)
    }

    private var color: Color {
        if model.status?.drivable == true && model.videoLive { return .green }
        if model.status?.reconnecting == true { return .yellow }
        return .red
    }

    private var label: String {
        guard let status = model.status else { return "未连接" }
        if status.drivable && model.videoLive { return "可操作 · H.264" }
        return status.deviceState
    }
}

struct Toolbar: View {
    let model: RemoteModel
    @Binding var typing: Bool
    @Binding var showSettings: Bool

    var body: some View {
        let handedOver = model.status?.humanHandoff == true
        HStack {
            ToolButton(title: "主屏幕", symbol: "house") { model.send(.home) }
            ToolButton(title: "键盘", symbol: "keyboard") { typing = true }
            if handedOver || model.status?.released == true {
                ToolButton(title: "连接手机", symbol: "bolt.horizontal") { model.connectPhone() }
            } else {
                ToolButton(title: "交还", symbol: "iphone.and.arrow.forward") { model.handBack() }
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
    let title: String
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

struct TypeSheet: View {
    let model: RemoteModel
    @State private var text = ""
    @FocusState private var focused: Bool
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                Text("先在手机画面上点一下输入框，再在这里输入；支持中文。")
                    .font(.footnote).foregroundStyle(.secondary)
                TextField("要输入的文字", text: $text, axis: .vertical)
                    .textFieldStyle(.roundedBorder)
                    .focused($focused)
                    .lineLimit(1...5)
                    .submitLabel(.send)
                    .onSubmit(send)
                Spacer()
            }
            .padding()
            .navigationTitle("输入文字")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("关闭") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("发送", action: send).disabled(text.isEmpty)
                }
            }
            .onAppear { focused = true }
        }
        .presentationDetents([.height(260)])
    }

    private func send() {
        guard !text.isEmpty else { return }
        model.send(.text(text))
        text = ""
    }
}

struct SettingsSheet: View {
    let model: RemoteModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section("连接") {
                    LabeledContent("服务地址", value: model.address)
                    LabeledContent("服务版本", value: model.status?.version ?? "—")
                    LabeledContent("设备状态", value: model.status?.deviceState ?? "—")
                }
                Section {
                    Button("断开") { model.disconnect(); dismiss() }
                    Button("忘记这台 Mac", role: .destructive) { model.forget(); dismiss() }
                }
            }
            .navigationTitle("设置")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("完成") { dismiss() } } }
        }
    }
}
