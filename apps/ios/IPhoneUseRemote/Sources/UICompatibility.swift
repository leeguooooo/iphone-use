import SwiftUI

struct AppNavigationStack<Content: View>: View {
    let content: Content
    init(@ViewBuilder content: () -> Content) { self.content = content() }
    @ViewBuilder var body: some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { NavigationStack { content } }
        else { NavigationView { content }.navigationViewStyle(.stack) }
    }
}

struct AppAxisStack<Content: View>: View {
    let horizontal: Bool
    let spacing: CGFloat
    let content: Content
    init(horizontal: Bool, spacing: CGFloat, @ViewBuilder content: () -> Content) {
        self.horizontal = horizontal; self.spacing = spacing; self.content = content()
    }
    @ViewBuilder var body: some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI {
            let layout = horizontal ? AnyLayout(HStackLayout(spacing: spacing)) : AnyLayout(VStackLayout(spacing: spacing))
            layout { content }
        } else if horizontal { HStack(spacing: spacing) { content } }
        else { VStack(spacing: spacing) { content } }
    }
}

struct AppChangeModifier<Value: Equatable>: ViewModifier {
    let value: Value
    let action: (Value, Value) -> Void
    @State var previous: Value
    func body(content: Content) -> some View {
        content.onChange(of: value) { next in
            let old = previous
            previous = next
            action(old, next)
        }
    }
}

extension View {
    func appOnChange<Value: Equatable>(of value: Value, perform: @escaping (Value, Value) -> Void) -> some View {
        modifier(AppChangeModifier(value: value, action: perform, previous: value))
    }
    func appOnChange<Value: Equatable>(of value: Value, perform: @escaping () -> Void) -> some View {
        appOnChange(of: value) { _, _ in perform() }
    }
    @ViewBuilder func appNavigationDestination<Destination: View>(isPresented: Binding<Bool>, @ViewBuilder destination: @escaping () -> Destination) -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { navigationDestination(isPresented: isPresented, destination: destination) }
        else { background(NavigationLink(destination: destination(), isActive: isPresented) { EmptyView() }.hidden()) }
    }
    @ViewBuilder func appSystemOverlays(hidden: Bool) -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { persistentSystemOverlays(hidden ? .hidden : .automatic) }
        else { statusBar(hidden: hidden) }
    }
}

extension Animation {
    static var appSnappy: Animation { appSnappy(duration: 0.35) }
    static func appSnappy(duration: Double) -> Animation {
        if #available(iOS 17, *), !AppCompatibility.forceLegacyUI { return .snappy(duration: duration) }
        return .spring(response: duration, dampingFraction: 0.86)
    }
}

struct AppUnavailableView: View {
    let title: LocalizedStringKey
    let systemImage: String
    let description: Text?
    init(_ title: LocalizedStringKey, systemImage: String, description: Text? = nil) {
        self.title = title; self.systemImage = systemImage; self.description = description
    }
    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: systemImage).font(.system(size: 44)).foregroundStyle(.secondary)
            Text(title).font(.title2.bold())
            description?.foregroundStyle(.secondary).multilineTextAlignment(.center)
        }.padding(24).frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct AppLabeledContent: View {
    let title: Text
    let value: String
    init(_ title: String, value: String) { self.title = Text(LocalizedStringKey(title)); self.value = value }
    var body: some View { HStack { title; Spacer(); Text(value).foregroundStyle(.secondary) } }
}

struct AppTextField: View {
    let title: LocalizedStringKey
    @Binding var text: String
    let axis: Axis
    init(_ title: LocalizedStringKey, text: Binding<String>, axis: Axis) {
        self.title = title; self._text = text; self.axis = axis
    }
    @ViewBuilder var body: some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { TextField(title, text: $text, axis: axis) }
        else {
            TextEditor(text: $text)
                .frame(minHeight: 36, maxHeight: 120)
                .overlay(alignment: .topLeading) {
                    if text.isEmpty { Text(title).foregroundStyle(.secondary).padding(.top, 8).allowsHitTesting(false) }
                }
                .accessibilityLabel(Text(title))
        }
    }
}

private struct AppGeometryKey<Value: Equatable>: PreferenceKey {
    static var defaultValue: Value? { nil }
    static func reduce(value: inout Value?, nextValue: () -> Value?) { value = nextValue() ?? value }
}

extension View {
    func appLineLimit(_ lines: ClosedRange<Int>) -> some View { lineLimit(lines.upperBound) }
    func appOnGeometryChange<Value: Equatable>(for: Value.Type, of transform: @escaping (GeometryProxy) -> Value, action: @escaping (Value) -> Void) -> some View {
        background(GeometryReader { proxy in Color.clear.preference(key: AppGeometryKey<Value>.self, value: transform(proxy)) })
            .onPreferenceChange(AppGeometryKey<Value>.self) { value in if let value { action(value) } }
    }
    @ViewBuilder func appMenuOrder() -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { menuOrder(.fixed) } else { self }
    }
    @ViewBuilder func appCircleButtonShape() -> some View {
        if #available(iOS 17, *), !AppCompatibility.forceLegacyUI { buttonBorderShape(.circle) } else { buttonBorderShape(.capsule) }
    }
}

private struct AppScrollConfiguration: UIViewRepresentable {
    var keyboard = false
    func makeUIView(context: Context) -> ConfigurationView { ConfigurationView(keyboard: keyboard) }
    func updateUIView(_ view: ConfigurationView, context: Context) { view.keyboard = keyboard; view.configure() }
    final class ConfigurationView: UIView {
        var keyboard: Bool
        init(keyboard: Bool) { self.keyboard = keyboard; super.init(frame: .zero) }
        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
        override func didMoveToWindow() { super.didMoveToWindow(); DispatchQueue.main.async { [weak self] in self?.configure() } }
        func configure() {
            var view = superview
            while let parent = view {
                if let scroll = parent as? UIScrollView {
                    if keyboard { scroll.keyboardDismissMode = .interactive }
                    else { scroll.alwaysBounceVertical = false }
                    return
                }
                view = parent.superview
            }
        }
    }
}

private struct AppSheetConfiguration: UIViewRepresentable {
    func makeUIView(context: Context) -> SheetView { SheetView() }
    func updateUIView(_ view: SheetView, context: Context) { view.configure() }
    final class SheetView: UIView {
        override func didMoveToWindow() { super.didMoveToWindow(); DispatchQueue.main.async { [weak self] in self?.configure() } }
        func configure() {
            var responder: UIResponder? = self
            while let next = responder?.next {
                if let controller = next as? UIViewController {
                    var candidate: UIViewController? = controller
                    while let parent = candidate {
                        if let sheet = parent.presentationController as? UISheetPresentationController {
                            sheet.detents = [.medium(), .large()]
                            sheet.prefersGrabberVisible = true
                            return
                        }
                        candidate = parent.parent
                    }
                }
                responder = next
            }
        }
    }
}

extension View {
    @ViewBuilder func appDismissKeyboardOnScroll() -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { scrollDismissesKeyboard(.interactively) }
        else { background(AppScrollConfiguration(keyboard: true)) }
    }
    @ViewBuilder func appBounceBasedOnSize() -> some View {
        if #available(iOS 16.4, *), !AppCompatibility.forceLegacyUI { scrollBounceBehavior(.basedOnSize) }
        else { background(AppScrollConfiguration()) }
    }
    @ViewBuilder func appMediumLargeSheet() -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { presentationDetents([.medium, .large]).presentationDragIndicator(.visible) }
        else { background(AppSheetConfiguration()) }
    }
}

struct AppPasteButton: View {
    let onPaste: ([String]) -> Void
    init(payloadType: String.Type, onPaste: @escaping ([String]) -> Void) { self.onPaste = onPaste }
    @ViewBuilder var body: some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { PasteButton(payloadType: String.self, onPaste: onPaste) }
        else {
            Button { if let text = UIPasteboard.general.string { onPaste([text]) } }
                label: { Label("粘贴", systemImage: "doc.on.clipboard") }
        }
    }
}

extension View {
    @ViewBuilder func appFlowPickerStyle() -> some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { pickerStyle(.navigationLink) } else { pickerStyle(.menu) }
    }
}

enum AppCompatibility {
    static var forceLegacyUI: Bool {
        #if DEBUG
        UserDefaults.standard.bool(forKey: "legacyUI")
        #else
        false
        #endif
    }
}

struct AppShareLink<Label: View>: View {
    let item: URL
    let label: Label
    @State private var presenting = false
    init(item: URL, @ViewBuilder label: () -> Label) { self.item = item; self.label = label() }
    @ViewBuilder var body: some View {
        if #available(iOS 16, *), !AppCompatibility.forceLegacyUI { ShareLink(item: item) { label } }
        else { Button { presenting = true } label: { label }.sheet(isPresented: $presenting) { AppShareSheet(item: item) } }
    }
}
private struct AppShareSheet: UIViewControllerRepresentable {
    let item: URL
    func makeUIViewController(context: Context) -> UIActivityViewController {
        let controller = UIActivityViewController(activityItems: [item], applicationActivities: nil)
        controller.popoverPresentationController?.sourceView = controller.view
        return controller
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}

extension AppShareLink where Label == SwiftUI.Label<Text, Image> {
    init(item: URL) {
        self.item = item
        self.label = SwiftUI.Label("分享", systemImage: "square.and.arrow.up")
    }
}
