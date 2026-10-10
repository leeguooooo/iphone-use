import SwiftUI
import UIKit

// The app's small design system: colors (dark first, each with a light
// twin), spacing, radii, and the few shared pieces every screen is built
// from — chips, glass buttons, the status dot — so the remote, the grid,
// the demo and the sheets look like one app.

enum Theme {
    // MARK: colors

    /// Behind the phone's picture: near black, never pure gray, so the
    /// picture's own blacks still read as the phone's screen.
    static let stage = Color(uiColor: UIColor { $0.userInterfaceStyle == .dark
        ? UIColor(red: 0.035, green: 0.035, blue: 0.045, alpha: 1)
        : UIColor(red: 0.93, green: 0.93, blue: 0.95, alpha: 1) })
    /// Screens with content on them (grid, onboarding).
    static let canvas = Color(uiColor: UIColor { $0.userInterfaceStyle == .dark
        ? UIColor(red: 0.055, green: 0.055, blue: 0.066, alpha: 1)
        : UIColor.systemGroupedBackground })
    /// Cards and tiles on the canvas.
    static let surface = Color(uiColor: UIColor { $0.userInterfaceStyle == .dark
        ? UIColor(red: 0.11, green: 0.11, blue: 0.125, alpha: 1)
        : UIColor.secondarySystemGroupedBackground })
    static let hairline = Color(uiColor: UIColor { $0.userInterfaceStyle == .dark
        ? UIColor.white.withAlphaComponent(0.10)
        : UIColor.black.withAlphaComponent(0.08) })
    /// The brand: the periwinkle of the app icon.
    static let accent = Color(uiColor: UIColor { $0.userInterfaceStyle == .dark
        ? UIColor(red: 0.45, green: 0.53, blue: 1.0, alpha: 1)
        : UIColor(red: 0.29, green: 0.36, blue: 0.95, alpha: 1) })
    static var uiAccent: UIColor { UIColor(red: 0.45, green: 0.53, blue: 1.0, alpha: 1) }

    static let ok = Color(red: 0.19, green: 0.82, blue: 0.35)
    static let busy = Color(red: 1.0, green: 0.76, blue: 0.20)
    static let attention = Color(red: 1.0, green: 0.62, blue: 0.04)
    static let down = Color(red: 1.0, green: 0.27, blue: 0.23)

    static func color(_ tone: ConnectionPresentation.Tone) -> Color {
        switch tone {
        case .ok: return ok
        case .busy: return busy
        case .attention: return attention
        case .down: return down
        }
    }

    // MARK: spacing and radii

    enum Space {
        static let xs: CGFloat = 4
        static let s: CGFloat = 8
        static let m: CGFloat = 12
        static let l: CGFloat = 16
        static let xl: CGFloat = 24
        static let xxl: CGFloat = 32
    }

    enum Radius {
        static let control: CGFloat = 14
        static let card: CGFloat = 20
        static let tile: CGFloat = 18
    }

    /// How round a phone's own screen is, as a share of its short side, so
    /// the picture has the corners of the phone it shows: about 0.12 on
    /// Face ID iPhones (tall screens), nearly square on Home-button ones.
    static func screenCornerRatio(for size: CGSize) -> CGFloat {
        let long = max(size.width, size.height)
        let short = min(size.width, size.height)
        guard short > 0 else { return 0.12 }
        return long / short > 1.9 ? 0.12 : 0.025
    }
}

// MARK: - Chips

/// A small capsule label: route (局域网 / 外网), lock readiness, a gesture's
/// result, demo, sync. One shape everywhere; the tone says how much it matters.
struct Chip: View {
    enum Style { case tinted, filled, glass }

    let text: String
    var symbol: String?
    var color: Color = .secondary
    var style: Style = .tinted

    var body: some View {
        HStack(spacing: 4) {
            if let symbol {
                Image(systemName: symbol).imageScale(.small).accessibilityHidden(true)
            }
            Text(text).lineLimit(1)
        }
        .font(.caption2.weight(.semibold))
        .padding(.horizontal, 7).padding(.vertical, 3)
        .foregroundStyle(style == .filled ? AnyShapeStyle(Color.white) : AnyShapeStyle(color))
        .background {
            switch style {
            case .tinted: Capsule().fill(color.opacity(0.18))
            case .filled: Capsule().fill(color.opacity(0.9))
            case .glass: Capsule().fill(.ultraThinMaterial)
            }
        }
        .fixedSize()
    }
}

/// The device's health as a dot; it breathes while something is in progress.
struct HealthDot: View {
    let health: DeviceSession.Health
    var size: CGFloat = 8
    @State private var pulse = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Circle()
            .fill(Theme.color(health))
            .frame(width: size, height: size)
            .overlay {
                if health == .busy && !reduceMotion {
                    Circle().stroke(Theme.color(health), lineWidth: 1.5)
                        .scaleEffect(pulse ? 2.2 : 1)
                        .opacity(pulse ? 0 : 0.8)
                        .onAppear {
                            withAnimation(.easeOut(duration: 1.2).repeatForever(autoreverses: false)) { pulse = true }
                        }
                        .onDisappear { pulse = false }
                }
            }
            .accessibilityHidden(true)
    }
}

// MARK: - Glass controls

/// The round glass button of the remote's chrome.
struct GlassIconButton: View {
    let symbol: String
    let label: LocalizedStringKey
    var size: CGFloat = 40
    var prominent = false
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: size * 0.4, weight: .semibold))
                .frame(width: size, height: size)
                .background(prominent ? AnyShapeStyle(Theme.accent) : AnyShapeStyle(.ultraThinMaterial), in: Circle())
                .overlay(Circle().strokeBorder(Theme.hairline))
                .contentShape(Circle())
        }
        .buttonStyle(PressableStyle())
        .foregroundStyle(prominent ? Color.white : Color.primary)
        .accessibilityLabel(Text(label))
        .accessibilityIdentifier(symbol)
    }
}

/// Shrinks a little under the finger, so every control answers at once.
struct PressableStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.92 : 1)
            .opacity(configuration.isPressed ? 0.8 : 1)
            .animation(.spring(response: 0.25, dampingFraction: 0.7), value: configuration.isPressed)
    }
}

/// The big buttons of onboarding and empty states.
struct ProminentButtonStyle: ButtonStyle {
    var filled = true

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.headline)
            .frame(maxWidth: .infinity, minHeight: 52)
            .foregroundStyle(filled ? Color.white : Color.primary)
            .background(filled ? AnyShapeStyle(Theme.accent) : AnyShapeStyle(Theme.surface),
                        in: RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous))
            .overlay {
                if !filled {
                    RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous)
                        .strokeBorder(Theme.hairline)
                }
            }
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
            .opacity(configuration.isPressed ? 0.85 : 1)
            .animation(.spring(response: 0.25, dampingFraction: 0.7), value: configuration.isPressed)
    }
}

extension View {
    /// Glass under a piece of chrome floating over the stage.
    func glassBackground<S: Shape>(_ shape: S) -> some View {
        background(.ultraThinMaterial, in: shape)
            .overlay(shape.stroke(Theme.hairline, lineWidth: 1))
    }
}

// MARK: - Haptics

@MainActor
enum Haptics {
    static func hold() { UIImpactFeedbackGenerator(style: .medium).impactOccurred() }
    static func tick() { UISelectionFeedbackGenerator().selectionChanged() }
    static func warning() { UINotificationFeedbackGenerator().notificationOccurred(.warning) }
    static func light() { UIImpactFeedbackGenerator(style: .light).impactOccurred(intensity: 0.7) }
}
