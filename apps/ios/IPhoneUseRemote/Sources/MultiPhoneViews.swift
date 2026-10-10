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
                    ContentUnavailableView {
                        Label("没有显示的手机", systemImage: "rectangle.grid.2x2")
                    } description: {
                        Text("所有手机都从总览隐藏了。在「设备」里打开「显示在总览」。")
                    } actions: {
                        Button("打开设备列表") { showDevices = true }
                            .buttonStyle(.borderedProminent)
                    }
                    .padding(.top, 80)
                } else {
                    LazyVGrid(columns: columns, spacing: Theme.Space.l) {
                        ForEach(tiles) { session in
                            DeviceTile(app: app, session: session)
                        }
                        AddPhoneTile(onScan: { scanning = true }, onManual: { adding = true })
                    }
                    .padding(.horizontal, Theme.Space.l)
                    .padding(.top, Theme.Space.s)
                    .padding(.bottom, Theme.Space.xl)
                }
            }
            // Pull down: every waiting phone tries again now.
            .refreshable { await app.refreshAll() }
            .background(Theme.canvas.ignoresSafeArea())
            .navigationTitle("我的手机")
            .navigationBarTitleDisplayMode(.large)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { showDevices = true } label: { Label("设备", systemImage: "list.bullet") }
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button { showSync = true } label: { Label("同步操作", systemImage: "square.on.square.dashed") }
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
        [GridItem(.adaptive(minimum: sizeClass == .regular ? 190 : 150), spacing: Theme.Space.l)]
    }
}

/// The last tile: add another phone (scan; hold for typing an address).
struct AddPhoneTile: View {
    let onScan: () -> Void
    let onManual: () -> Void

    var body: some View {
        Button(action: onScan) {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                VStack(spacing: Theme.Space.s) {
                    Image(systemName: "qrcode.viewfinder")
                        .font(.title2.weight(.semibold))
                        .frame(width: 52, height: 52)
                        .background(Theme.accent.opacity(0.16), in: Circle())
                        .foregroundStyle(Theme.accent)
                    Text("添加手机").font(.subheadline.weight(.semibold))
                    Text("扫它在 Mac 上的二维码").font(.caption2).foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .padding(Theme.Space.m)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .aspectRatio(9.0 / 19.5, contentMode: .fit)
                .background {
                    TileShape()
                        .stroke(style: StrokeStyle(lineWidth: 1.5, dash: [6, 5]))
                        .foregroundStyle(Color.secondary.opacity(0.5))
                }
                // The labels under the other tiles.
                Color.clear.frame(height: 36)
            }
        }
        .buttonStyle(PressableStyle())
        .foregroundStyle(.primary)
        .contextMenu {
            Button(action: onScan) { Label("扫码添加", systemImage: "qrcode.viewfinder") }
            Button(action: onManual) { Label("手动添加", systemImage: "keyboard") }
        }
        .accessibilityLabel(Text("添加手机"))
        .accessibilityHint(Text("扫描 Mac 上的二维码；长按可以手动输入地址"))
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
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                TilePicture(session: session)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        HealthDot(health: session.health)
                        Text(session.name).font(.subheadline.weight(.semibold)).lineLimit(1)
                    }
                    HStack(spacing: 4) {
                        let p = session.presentation()
                        Text(p.short)
                            .font(.caption)
                            .foregroundStyle(p.tone == .ok ? AnyShapeStyle(.secondary) : AnyShapeStyle(Theme.color(p.tone)))
                            .lineLimit(1)
                        if let route = session.routeLabel {
                            Chip(text: route, color: .secondary)
                        }
                    }
                }
                .padding(.horizontal, 2)
            }
        }
        .buttonStyle(PressableStyle())
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
                        ProgressView().tint(Theme.color(p.tone))
                    } else {
                        Image(systemName: p.symbol).font(.title2).foregroundStyle(Theme.color(p.tone))
                    }
                    Text(p.short).font(.caption2.weight(.medium)).multilineTextAlignment(.center)
                        .foregroundStyle(.secondary)
                }
                .padding(8)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black.opacity(p.placement == .cover ? 0.6 : 0))
                .allowsHitTesting(false)
                .transition(.opacity)
            }
            if let note = session.delivery {
                DeliveryBadge(outcome: note.outcome).padding(6)
                    .transition(.scale.combined(with: .opacity))
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
        .background(Color(white: 0.07))
        .clipShape(TileShape())
        .overlay(TileShape().stroke(Theme.hairline, lineWidth: 1))
        .shadow(color: .black.opacity(0.25), radius: 10, y: 4)
        .animation(.snappy(duration: 0.25), value: session.delivery)
        .animation(.easeInOut(duration: 0.25), value: session.videoLive)
    }
}

/// A tile's outline: the phone's own rounded corners at tile size.
struct TileShape: Shape {
    func path(in rect: CGRect) -> Path {
        let radius = Theme.screenCornerRatio(for: rect.size) * min(rect.width, rect.height)
        return Path(roundedRect: rect, cornerRadius: max(radius, 10), style: .continuous)
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
                Chip(text: badge.title, symbol: badge.symbol,
                     color: badge.urgent ? Theme.attention : Color(white: 0.45), style: .filled)
            } else {
                Label(badge.sentence, systemImage: badge.symbol)
                    .font(.caption)
                    .foregroundStyle(badge.urgent ? Theme.attention : Color.secondary)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(badge.sentence))
    }
}

/// A gesture's result on one phone: ok / not sent / owned / unknown / failed.
struct DeliveryBadge: View {
    let outcome: DeliveryOutcome

    var body: some View {
        Chip(text: outcome.badge, symbol: symbol, color: color, style: .filled)
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
        case .ok: return Theme.ok
        case .notSent: return .gray
        case .owned: return .purple
        case .outcomeUnknown: return Theme.attention
        case .failed: return Theme.down
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
    @State private var immersive = false
    @State private var showSettings = false
    @State private var zoom: CGFloat = 1
    @State private var resetZoom = 0
    @State private var queue: TypingQueue

    init(app: AppModel, lead: DeviceSession, group: AppModel.SyncGroup) {
        self.app = app
        self.lead = lead
        self.group = group
        _queue = State(initialValue: TypingQueue { [weak app, weak lead] action in
            guard let app, let lead else { return }
            await app.perform(action, from: lead)
        })
    }

    private var followers: [DeviceSession] {
        group.followers.compactMap { app.session($0) }
    }

    var body: some View {
        RemoteScaffold(immersive: immersive, typing: typing, onExitImmersive: { immersive = false }) { axis in
            RemoteTopBar(axis: axis, backSymbol: "xmark", backLabel: "退出同步", zoom: zoom,
                         onBack: { app.stopSync() },
                         onResetZoom: { resetZoom += 1 },
                         onImmersive: { immersive = true }) {
                Button { showSettings = true } label: { SessionBadge(session: lead, axis: axis) }
                    .buttonStyle(PressableStyle())
                    .foregroundStyle(.primary)
            }
        } screen: {
            RemoteStage(app: app, session: lead, zoom: $zoom, resetZoom: resetZoom) { app.dispatch($0, from: lead) }
        } accessory: { axis in
            if !immersive { followerStrip(axis) }
        } keys: { axis in
            KeyBar(axis: axis,
                   onBack: { app.dispatch(.back, from: lead) },
                   onHome: { app.dispatch(.home, from: lead) },
                   onSearch: { app.dispatch(.spotlight, from: lead) },
                   onKeyboard: { typing = true }) {
                Button { app.connectAll(group.members) } label: { Label("全部连接", systemImage: "bolt.horizontal") }
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
                Divider()
                Button(role: .destructive) { app.stopSync() } label: { Label("退出同步", systemImage: "xmark.circle") }
            }
        } keyboard: {
            LiveKeyboardBar(queue: queue, targets: group.members.count, onDone: { typing = false })
        }
        .sheet(isPresented: $showSettings) { SettingsSheet(app: app, session: lead) }
    }

    @ViewBuilder
    private func followerStrip(_ axis: Axis) -> some View {
        let header = Chip(text: String(localized: "同步 · \(group.members.count) 台"), symbol: "square.on.square.dashed",
                          color: Theme.attention)
        if axis == .horizontal {
            VStack(alignment: .leading, spacing: 6) {
                header.padding(.horizontal, Theme.Space.m)
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: Theme.Space.s) { followerTiles(width: 58) }
                        .padding(.horizontal, Theme.Space.m)
                }
            }
            .frame(height: 158)
        } else {
            VStack(spacing: 6) {
                header
                ScrollView {
                    VStack(spacing: Theme.Space.s) { followerTiles(width: 64) }
                }
            }
            .frame(width: 84)
            .padding(.vertical, Theme.Space.s)
        }
    }

    @ViewBuilder
    private func followerTiles(width: CGFloat) -> some View {
        ForEach(followers) { session in
            VStack(spacing: 3) {
                TilePicture(session: session)
                HStack(spacing: 4) {
                    HealthDot(health: session.health, size: 6)
                    Text(session.name).font(.caption2).lineLimit(1)
                }
            }
            .frame(width: width)
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
