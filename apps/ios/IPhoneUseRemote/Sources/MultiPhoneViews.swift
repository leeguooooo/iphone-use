import SwiftUI

// MARK: - Overview grid

/// Every phone shown in the grid at once, each a live tile. Tap a tile to
/// drive that phone full screen.
struct DeviceGridView: View {
    @Bindable var app: AppModel
    @State private var showDevices = false
    @State private var showSync = false
    @State private var adding = false
    @State private var scanning = false
    @State private var manualAfterScan = false
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        NavigationStack {
            ScrollView {
                OfflineBanner(online: app.online)
                let tiles = app.gridSessions
                if tiles.isEmpty {
                    ContentUnavailableView("没有显示的手机", systemImage: "rectangle.grid.2x2",
                                           description: Text("在「设备」里打开「显示在总览」。"))
                        .padding(.top, 80)
                } else {
                    LazyVGrid(columns: columns, spacing: 14) {
                        ForEach(tiles) { session in
                            DeviceTile(app: app, session: session)
                        }
                    }
                    .padding(.horizontal)
                    .padding(.bottom, 24)
                }
            }
            // Pull down: every waiting phone tries again now.
            .refreshable { await app.refreshAll() }
            .background(Color.black.ignoresSafeArea())
            .navigationTitle("我的手机")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { showDevices = true } label: { Label("设备", systemImage: "list.bullet") }
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button { showSync = true } label: { Label("同步", systemImage: "rectangle.on.rectangle") }
                        .disabled(app.devices.count < 2)
                    Menu {
                        Button { scanning = true } label: { Label("扫码添加", systemImage: "qrcode.viewfinder") }
                        Button { adding = true } label: { Label("手动添加", systemImage: "keyboard") }
                    } label: {
                        Label("添加手机", systemImage: "plus")
                    }
                }
            }
            .overlay { ToastLayer(text: app.toast) }
            .sheet(isPresented: $showDevices) { DevicesSheet(app: app) }
            .sheet(isPresented: $showSync) { SyncSetupSheet(app: app) }
            .sheet(isPresented: $adding) { ConnectView(app: app) { adding = false } }
            .fullScreenCover(isPresented: $scanning, onDismiss: {
                if manualAfterScan { manualAfterScan = false; adding = true }
            }) {
                ScanSheet(onFound: { link in Task { await app.pair(link) } }, onManual: { manualAfterScan = true })
            }
        }
    }

    /// Two across on an iPhone, more on an iPad.
    private var columns: [GridItem] {
        [GridItem(.adaptive(minimum: sizeClass == .regular ? 200 : 150), spacing: 14)]
    }
}

/// One phone in the grid: its live picture, name, state and last sync result.
struct DeviceTile: View {
    @Bindable var app: AppModel
    let session: DeviceSession
    @State private var renaming = false
    @State private var newName = ""
    @State private var confirmForget = false

    var body: some View {
        Button {
            app.focusedID = session.id
        } label: {
            VStack(alignment: .leading, spacing: 6) {
                TilePicture(session: session)
                HStack(spacing: 6) {
                    HealthDot(health: session.health)
                    Text(session.name).font(.subheadline.weight(.semibold)).lineLimit(1)
                }
                Text(session.secondaryName)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Text(session.shortState)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(tileLabel))
        .accessibilityHint(Text("打开全屏操作"))
        .accessibilityAddTraits(.isButton)
        .contextMenu {
            Button { newName = session.record.customName ?? ""; renaming = true } label: { Label("重命名", systemImage: "pencil") }
            if session.status?.released == true || session.status?.humanHandoff == true {
                Button { session.connectPhone() } label: { Label("连接手机", systemImage: "bolt.horizontal") }
            } else if session.phase == .connected {
                Button { session.handBack() } label: { Label("交还", systemImage: "iphone.and.arrow.forward") }
            }
            if case .failed = session.phase {
                Button { Task { await session.connect(password: nil) } } label: { Label("立即重试", systemImage: "arrow.clockwise") }
            }
            Button { app.editing = .init(id: session.id) } label: { Label("编辑地址和密码", systemImage: "key") }
            Button { app.setShowInGrid(session.id, false) } label: { Label("从总览隐藏", systemImage: "eye.slash") }
            Button(role: .destructive) { confirmForget = true } label: { Label("忘记这台手机", systemImage: "trash") }
        }
        .alert("重命名", isPresented: $renaming) {
            TextField(session.record.defaultDisplayName, text: $newName)
            Button("取消", role: .cancel) {}
            Button("完成") { app.rename(session.id, to: newName) }
        }
        .confirmationDialog("忘记这台手机？", isPresented: $confirmForget, titleVisibility: .visible) {
            Button("忘记", role: .destructive) { app.remove(session.id) }
        } message: {
            Text("会删掉这台手机的配对，之后要重新扫码才能连接。")
        }
    }
}

extension DeviceTile {
    /// Name, state, and whether it locks on its own, for VoiceOver.
    var tileLabel: String {
        let base = "\(session.name)，\(session.presentation().title)"
        guard let badge = session.status?.lockBadge else { return base }
        return "\(base)，\(badge.sentence)"
    }
}

/// A tile's picture: the live stream, the reason there is none, the last
/// sync result as a badge, and a lock badge when the phone locks on its own.
struct TilePicture: View {
    let session: DeviceSession

    var body: some View {
        ZStack(alignment: .topTrailing) {
            LiveVideo(session: session)
            let p = session.presentation()
            // The same state the full screen shows, in a tile's words.
            if p.placement == .cover || !session.videoLive {
                VStack(spacing: 6) {
                    if p.progress {
                        ProgressView()
                    } else {
                        Image(systemName: p.symbol).font(.title2)
                    }
                    Text(p.short).font(.caption2).multilineTextAlignment(.center)
                }
                .foregroundStyle(.secondary)
                .padding(8)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black.opacity(p.placement == .cover ? 0.55 : 0))
                .allowsHitTesting(false)
            }
            if let note = session.delivery {
                DeliveryBadge(outcome: note.outcome).padding(6)
            }
            if let lock = session.status?.lockBadge {
                LockBadgeView(badge: lock, compact: true)
                    .padding(6)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
                    .allowsHitTesting(false)
            }
        }
        .aspectRatio(9.0 / 19.5, contentMode: .fit)
        .frame(maxWidth: .infinity)
        .background(Color(white: 0.08))
        .clipShape(RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(.white.opacity(0.12)))
        .animation(.easeInOut(duration: 0.2), value: session.delivery)
    }
}

/// The phone locks on its own when idle: needs a person (orange) or unlocks by
/// itself (gray). `compact` is the tile's capsule; the list row says it in words.
struct LockBadgeView: View {
    let badge: LockBadge
    var compact = true

    var body: some View {
        Group {
            if compact {
                Label(badge.title, systemImage: badge.symbol)
                    .font(.caption2.weight(.semibold))
                    .padding(.horizontal, 6).padding(.vertical, 3)
                    .background((badge.urgent ? Color.orange : Color.gray).opacity(0.85), in: Capsule())
                    .foregroundStyle(.white)
            } else {
                Label(badge.sentence, systemImage: badge.symbol)
                    .font(.caption)
                    .foregroundStyle(badge.urgent ? Color.orange : Color.secondary)
            }
        }
        .labelStyle(.titleAndIcon)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(badge.sentence))
    }
}

/// A gesture's result on one phone: ok / not sent / owned / unknown / failed.
struct DeliveryBadge: View {
    let outcome: DeliveryOutcome

    var body: some View {
        Label(outcome.badge, systemImage: symbol)
            .font(.caption2.weight(.semibold))
            .labelStyle(.titleAndIcon)
            .padding(.horizontal, 6).padding(.vertical, 3)
            .background(color.opacity(0.85), in: Capsule())
            .foregroundStyle(.white)
            .accessibilityLabel(Text(outcome.sentence))
    }

    private var symbol: String {
        switch outcome {
        case .ok: return "checkmark"
        case .notSent: return "arrow.uturn.backward"
        case .owned: return "person.badge.key"
        case .outcomeUnknown: return "questionmark"
        case .failed: return "xmark"
        }
    }

    private var color: Color {
        switch outcome {
        case .ok: return .green
        case .notSent: return .gray
        case .owned: return .purple
        case .outcomeUnknown: return .orange
        case .failed: return .red
        }
    }
}

// MARK: - Device list

/// Manage saved phones: rename, reorder, show or hide in the grid, forget, add.
struct DevicesSheet: View {
    @Bindable var app: AppModel
    @State private var adding = false
    @State private var scanning = false
    @State private var manualAfterScan = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section {
                    ForEach(app.devices) { record in
                        row(record)
                    }
                    .onDelete { offsets in
                        for index in offsets { app.remove(app.devices[index].id) }
                    }
                    .onMove { app.move(from: $0, to: $1) }
                } footer: {
                    Text("一台手机对应 Mac 上的一个 iphone-use 实例（各自的端口和二维码）。隐藏的手机不占画面，空闲时会自动释放。")
                }
                Section {
                    Button { scanning = true } label: { Label("扫码添加", systemImage: "qrcode.viewfinder") }
                    Button { adding = true } label: { Label("手动添加", systemImage: "keyboard") }
                }
            }
            .navigationTitle("设备")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) { EditButton() }
                ToolbarItem(placement: .confirmationAction) { Button("完成") { dismiss() } }
            }
            .sheet(isPresented: $adding) { ConnectView(app: app) { adding = false; dismiss() } }
            .fullScreenCover(isPresented: $scanning, onDismiss: {
                if manualAfterScan { manualAfterScan = false; adding = true }
            }) {
                ScanSheet(onFound: { link in
                    Task {
                        await app.pair(link)
                        dismiss()
                    }
                }, onManual: { manualAfterScan = true })
            }
        }
    }

    @ViewBuilder
    private func row(_ record: DeviceRecord) -> some View {
        let session = app.session(record.id)
        HStack(spacing: 10) {
            NavigationLink {
                DeviceEditForm(app: app, id: record.id)
            } label: {
                HStack(spacing: 10) {
                    if let session { HealthDot(health: session.health) }
                    VStack(alignment: .leading, spacing: 2) {
                        Text(record.displayName).font(.body)
                        Text(record.secondaryText).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                        if let session {
                            Text(session.shortState).font(.caption).foregroundStyle(.secondary)
                            if let lock = session.status?.lockBadge {
                                LockBadgeView(badge: lock, compact: false)
                            }
                        }
                    }
                }
            }
            .accessibilityHint(Text("编辑名称、地址和密码"))
            Toggle("显示在总览", isOn: Binding(
                get: { record.showInGrid },
                set: { app.setShowInGrid(record.id, $0) }))
                .labelsHidden()
                .fixedSize()
                .accessibilityLabel(Text("在总览显示 \(record.displayName)"))
        }
    }
}

// MARK: - Sync

/// Pick the lead and the followers for sync.
struct SyncSetupSheet: View {
    @Bindable var app: AppModel
    @State private var selected: Set<UUID> = []
    @State private var lead: UUID?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section {
                    ForEach(app.devices) { record in
                        let session = app.session(record.id)
                        Button {
                            toggle(record.id)
                        } label: {
                            HStack(spacing: 10) {
                                Image(systemName: selected.contains(record.id) ? "checkmark.circle.fill" : "circle")
                                    .foregroundStyle(selected.contains(record.id) ? Color.accentColor : .secondary)
                                if let session { HealthDot(health: session.health) }
                                VStack(alignment: .leading) {
                                    Text(record.displayName)
                                    if let session {
                                        Text(session.shortState).font(.caption).foregroundStyle(.secondary)
                                    }
                                }
                                Spacer()
                                if lead == record.id {
                                    Text("主控").font(.caption.weight(.semibold))
                                        .padding(.horizontal, 8).padding(.vertical, 3)
                                        .background(Color.accentColor.opacity(0.25), in: Capsule())
                                }
                            }
                        }
                        .foregroundStyle(.primary)
                        .accessibilityAddTraits(selected.contains(record.id) ? .isSelected : [])
                        .accessibilityValue(Text(lead == record.id ? "主控" : ""))
                        .swipeActions(edge: .leading) {
                            Button("设为主控") { setLead(record.id) }.tint(.accentColor)
                        }
                        .contextMenu {
                            Button("设为主控") { setLead(record.id) }
                        }
                    }
                } header: {
                    Text("选择手机（至少两台）")
                } footer: {
                    Text("在主控手机上的每个手势和输入的文字，会按相同的相对位置同时发到所有选中的手机，各自显示送达结果。没送达的不会自动重发，结果未知的也不会。被其它会话占用的手机会被跳过。长按或右滑可以换主控。")
                }
            }
            .navigationTitle("同步操作")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("取消") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("开始") {
                        guard let lead else { return }
                        app.startSync(lead: lead, followers: app.devices.map(\.id).filter { selected.contains($0) })
                        dismiss()
                    }
                    .disabled(lead == nil || selected.count < 2)
                }
            }
            .onAppear {
                let all = app.devices.map(\.id)
                selected = Set(all)
                lead = app.focusedID ?? all.first
            }
        }
    }

    private func toggle(_ id: UUID) {
        if selected.contains(id) {
            selected.remove(id)
            if lead == id { lead = app.devices.map(\.id).first { selected.contains($0) } }
        } else {
            selected.insert(id)
            if lead == nil { lead = id }
        }
    }

    private func setLead(_ id: UUID) {
        selected.insert(id)
        lead = id
    }
}

/// Sync in progress: the lead full size and touchable, the followers as
/// small live tiles that show each gesture's result.
struct SyncView: View {
    @Bindable var app: AppModel
    let lead: DeviceSession
    let group: AppModel.SyncGroup
    @State private var typing = false
    @Environment(\.horizontalSizeClass) private var sizeClass

    private var followers: [DeviceSession] {
        group.followers.compactMap { app.session($0) }
    }

    var body: some View {
        VStack(spacing: 6) {
            HStack(spacing: 8) {
                BackToGridButton(app: app) { app.stopSync() }
                    .accessibilityLabel(Text("退出同步"))
                Label("同步 · \(group.members.count) 台", systemImage: "rectangle.on.rectangle")
                    .font(.caption.weight(.semibold))
                    .padding(.horizontal, 10).padding(.vertical, 5)
                    .background(Color.orange.opacity(0.3), in: Capsule())
                StatusPill(session: lead, showName: true)
            }
            .padding(.horizontal)
            if sizeClass == .regular {
                HStack(spacing: 10) {
                    leadScreen
                    ScrollView {
                        VStack(spacing: 10) { followerTiles }
                    }
                    .frame(width: 170)
                }
                .padding(.horizontal, 8)
            } else {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) { followerTiles }
                        .padding(.horizontal)
                }
                .frame(height: 150)
                leadScreen
            }
            HStack {
                ToolButton(title: "主屏幕", symbol: "house") { app.dispatch(.home, from: lead) }
                ToolButton(title: "键盘", symbol: "keyboard") { typing = true }
                ToolButton(title: "全部连接", symbol: "bolt.horizontal") { app.connectAll(group.members) }
                ToolButton(title: "退出同步", symbol: "xmark.circle") { app.stopSync() }
            }
            .padding(.horizontal, 8).padding(.vertical, 6)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 18))
            .padding(.horizontal)
            .padding(.bottom, 6)
        }
        .background(Color.black.ignoresSafeArea())
        .animation(.easeInOut(duration: 0.2), value: lead.toast)
        .sheet(isPresented: $typing) {
            TypeSheet(targets: group.members.count) { app.dispatch(.text($0), from: lead) }
        }
    }

    private var leadScreen: some View {
        ZStack {
            RemoteScreen(session: lead) { app.dispatch($0, from: lead) }
            if let wireframe = lead.redactedImage {
                Image(uiImage: wireframe).resizable().scaledToFit().allowsHitTesting(false)
            }
            StatusOverlay(app: app, session: lead)
            ToastLayer(text: lead.toast)
        }
    }

    @ViewBuilder
    private var followerTiles: some View {
        ForEach(followers) { session in
            VStack(spacing: 3) {
                TilePicture(session: session)
                HStack(spacing: 4) {
                    HealthDot(health: session.health)
                    Text(session.name).font(.caption2).lineLimit(1)
                }
            }
            .frame(width: sizeClass == .regular ? 150 : 62)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(Text(followerLabel(session)))
        }
    }

    private func followerLabel(_ session: DeviceSession) -> String {
        if let note = session.delivery {
            return "\(session.name)，\(session.presentation().title)，\(note.outcome.badge)"
        }
        return "\(session.name)，\(session.presentation().title)"
    }
}
