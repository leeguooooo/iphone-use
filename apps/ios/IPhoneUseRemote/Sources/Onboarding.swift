import SwiftUI

/// First launch: what this app does, the three steps to pair, and the ways
/// in — scan (the obvious one), the demo, or typing an address.
struct WelcomeView: View {
    @Bindable var app: AppModel
    @State private var scanning = false
    @State private var manual = false
    @State private var manualAfterScan = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: Theme.Space.xl) {
                    PairingIllustration()
                        .frame(height: 170)
                        .padding(.top, Theme.Space.l)
                    VStack(spacing: Theme.Space.s) {
                        Text("Phone Use Remote")
                            .font(.largeTitle.weight(.bold))
                            .multilineTextAlignment(.center)
                        Text("在这里看到并操作连在你 Mac 上的 iPhone：点按、滑动、打字，就像拿在手里。")
                            .font(.body)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    VStack(alignment: .leading, spacing: Theme.Space.l) {
                        StepRow(number: 1, title: "在 Mac 上装好 iphone-use",
                                detail: "它在 Mac 上运行，用线或 Wi‑Fi 连着要操作的 iPhone。")
                        StepRow(number: 2, title: "打开它的网页，点「扫码」",
                                detail: "页面会显示一个二维码，5 分钟内有效。")
                        StepRow(number: 3, title: "用这里扫一下",
                                detail: "自动配对，不用输入地址和密码。iPhone 自带相机扫也可以。")
                    }
                    .padding(Theme.Space.l)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.Radius.card, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: Theme.Radius.card, style: .continuous).strokeBorder(Theme.hairline))
                    VStack(spacing: Theme.Space.m) {
                        Button { scanning = true } label: {
                            Label("扫码连接", systemImage: "qrcode.viewfinder")
                        }
                        .buttonStyle(ProminentButtonStyle())
                        .disabled(app.adding)
                        Button { app.startDemo() } label: {
                            Label("先试用演示", systemImage: "play.rectangle")
                        }
                        .buttonStyle(ProminentButtonStyle(filled: false))
                        Button("手动输入地址和密码") { manual = true }
                            .font(.subheadline.weight(.semibold))
                            .frame(minHeight: 44)
                        if let error = app.addError {
                            Label(error, systemImage: "exclamationmark.triangle.fill")
                                .font(.footnote)
                                .foregroundStyle(Theme.attention)
                                .multilineTextAlignment(.center)
                        }
                    }
                    Text("演示播放从真实 iPhone 录下的画面，不连接任何设备。")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .frame(maxWidth: 520)
                .padding(.horizontal, Theme.Space.xl)
                .padding(.bottom, Theme.Space.xxl)
                .frame(maxWidth: .infinity)
            }
            .scrollBounceBehavior(.basedOnSize)
            .background(Theme.canvas.ignoresSafeArea())
            .navigationDestination(isPresented: $manual) {
                ConnectView(app: app, onDone: nil, embedded: true)
            }
            .fullScreenCover(isPresented: $scanning, onDismiss: {
                // "Enter by hand" from the scanner, once the camera is gone.
                if manualAfterScan { manualAfterScan = false; manual = true }
            }) {
                ScanSheet(onFound: { link in Task { await app.pair(link) } },
                          onManual: { manualAfterScan = true })
            }
        }
    }
}

private struct StepRow: View {
    let number: Int
    let title: LocalizedStringKey
    let detail: LocalizedStringKey

    var body: some View {
        HStack(alignment: .top, spacing: Theme.Space.m) {
            Text("\(number)")
                .font(.subheadline.weight(.bold).monospacedDigit())
                .foregroundStyle(Theme.accent)
                .frame(width: 28, height: 28)
                .background(Theme.accent.opacity(0.16), in: Circle())
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.subheadline.weight(.semibold))
                Text(detail).font(.footnote).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// A Mac showing a QR code, an iPhone scanning it, and a dot travelling
/// between them: the pairing, at a glance.
struct PairingIllustration: View {
    @State private var travel = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(alignment: .center, spacing: 0) {
            ZStack {
                Image(systemName: "laptopcomputer")
                    .font(.system(size: 104, weight: .ultraLight))
                    .foregroundStyle(.secondary)
                Image(systemName: "qrcode")
                    .font(.system(size: 34, weight: .regular))
                    .foregroundStyle(.primary)
                    .offset(y: -9)
            }
            .frame(width: 130)
            ZStack {
                Capsule()
                    .stroke(style: StrokeStyle(lineWidth: 2, lineCap: .round, dash: [2, 7]))
                    .foregroundStyle(Theme.accent.opacity(0.6))
                    .frame(height: 2)
                Circle()
                    .fill(Theme.accent)
                    .frame(width: 9, height: 9)
                    .shadow(color: Theme.accent, radius: 6)
                    .offset(x: travel ? 28 : -28)
            }
            .frame(width: 70)
            ZStack {
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .fill(Theme.surface)
                    .frame(width: 62, height: 124)
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .strokeBorder(Color.secondary.opacity(0.6), lineWidth: 2)
                    .frame(width: 62, height: 124)
                Image(systemName: "viewfinder")
                    .font(.system(size: 34, weight: .light))
                    .foregroundStyle(Theme.accent)
                Image(systemName: "qrcode")
                    .font(.system(size: 16))
                    .foregroundStyle(.primary.opacity(0.8))
            }
        }
        .accessibilityHidden(true)
        .onAppear {
            guard !reduceMotion else { return }
            withAnimation(.easeInOut(duration: 1.4).repeatForever(autoreverses: true)) { travel = true }
        }
    }
}
