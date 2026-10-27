import UIKit
import CryptoKit
private final class SelectionField: UITextField {
  var selectionPolicy = ""
  var selectionCommands = 0
  var touchStarts = 0
  var repopulateOnFocus = false
  override func becomeFirstResponder() -> Bool {
    let result = super.becomeFirstResponder()
    if result && repopulateOnFocus {
      repopulateOnFocus = false
      text = "App refocus draft"
      sendActions(for: .editingChanged)
    }
    return result
  }
  override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
    touchStarts += 1
    super.touchesBegan(touches, with: event)
  }
  override func selectAll(_ sender: Any?) {
    selectionCommands += 1
    if selectionPolicy == "ignore-all" || (selectionPolicy == "ignore-once" && selectionCommands == 1) { return }
    if selectionPolicy == "selection-mutates" && selectionCommands == 1 {
      text = "new-draft-middle"
      sendActions(for: .editingChanged)
      return
    }
    if selectionPolicy == "selection-partial" {
      if let end = position(from: beginningOfDocument, offset: 3) { selectedTextRange = textRange(from: beginningOfDocument, to: end) }
      return
    }
    super.selectAll(sender)
  }
}
@main final class AppDelegate: UIResponder, UIApplicationDelegate {
  func application(_ application: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]?) -> Bool { true }
}
@objc(InputGuardSceneDelegate) final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?
  private let titleLabel = UILabel()
  private let a = SelectionField(), b = UITextField(), secure = UITextField(), multi = UITextView()
  private var timer: Timer?
  private var last: Data?
  private var events = ["a": 0, "b": 0, "secure": 0, "multi": 0]
  private var trigger = 0
  private var touchesAtShift = 0
  private var initialFocus = false
  private var keyboardShown = false
  private var activeSeed = "short"
  private func seededValue(_ seed: String) -> String {
    switch seed {
    case "long": return "queue-deadline-" + String(repeating: "字", count: 6000)
    case "middle", "ignore-once", "ignore-all", "selection-mutates", "selection-partial": return "old-draft-middle"
    case "unicode": return "旧🙂e\u{301}草稿"
    case "limit": return String(repeating: "字", count: 100)
    case "overlimit": return String(repeating: "字", count: 101)
    default: return "old"
    }
  }
  private var currentTrial = ""
  private var shiftFocus = false
  private var secureFocus = false
  private var advanceOnClear = false
  private var clearAdvance = false
  private var clearFault = ""
  private var touchesAtClear = -1
  private var clearOverlay: UIButton?
  private var overlayActions = 0
  private var trial: String { currentTrial.isEmpty ? option("--trial") : currentTrial }
  private func resetIfRequested() {
    let url = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].appendingPathComponent("paired-reset.json")
    guard let data = try? Data(contentsOf: url), let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let next = body["trial"] as? String, next != trial else { return }
    currentTrial = next
    titleLabel.text = "Paired fixture " + next
    shiftFocus = body["shift"] as? Bool ?? false
    secureFocus = body["secure"] as? Bool ?? false
    advanceOnClear = body["advance"] as? Bool ?? false
    clearFault = body["clear_fault"] as? String ?? ""
    clearAdvance = false
    touchesAtClear = -1
    clearOverlay?.removeFromSuperview(); clearOverlay = nil; overlayActions = 0
    activeSeed = body["seed"] as? String ?? "short"
    a.selectionPolicy = activeSeed; a.selectionCommands = 0
    a.isEnabled = true; a.isHidden = false; a.isSecureTextEntry = false; a.touchStarts = 0; a.repopulateOnFocus = false
    a.text = seededValue(activeSeed)
    b.text = advanceOnClear ? "keep-b" : ""; multi.text = ""; secure.text = "benchmark-secure-dummy"
    events = ["a": 0, "b": 0, "secure": 0, "multi": 0]; trigger = 0; touchesAtShift = 0; initialFocus = false
  }
  private func option(_ name: String) -> String {
    let args = ProcessInfo.processInfo.arguments
    guard let index = args.firstIndex(of: name), index + 1 < args.count else { return "" }
    return args[index + 1]
  }
  @objc private func changed(_ field: UITextField) {
    let key = field === a ? "a" : field === b ? "b" : "secure"
    events[key, default: 0] += 1
    if field === a && advanceOnClear && !clearAdvance && (a.text ?? "").isEmpty {
      clearAdvance = true
      touchesAtClear = a.touchStarts
      if clearFault == "repopulate" {
        a.text = "App restored draft"
        a.sendActions(for: .editingChanged)
      } else if clearFault == "disable" { a.isEnabled = false }
      else if clearFault == "hide" { a.isHidden = true }
      else if clearFault == "secure" { a.isSecureTextEntry = true }
      else if clearFault == "refocus-repopulate" { a.repopulateOnFocus = true }
      else if clearFault == "overlay", let view = window?.rootViewController?.view {
        let button = UIButton(type: .system)
        button.frame = a.convert(a.bounds, to: view)
        button.backgroundColor = .systemRed
        button.setTitle("Fixture action", for: .normal)
        button.accessibilityIdentifier = "clear-overlay-action"
        button.addTarget(self, action: #selector(overlayPressed), for: .touchUpInside)
        view.addSubview(button); clearOverlay = button
      }
      b.becomeFirstResponder()
    }
    if field === a && shiftFocus && trigger == 0,
       let text = a.text, text.hasPrefix("shift-focus-test-"), text.unicodeScalars.count >= 20 {
      trigger = text.unicodeScalars.count
      touchesAtShift = a.touchStarts
      b.becomeFirstResponder()
    }
  }
  @objc private func overlayPressed() { overlayActions += 1 }
  @objc private func keyboardDidShow(_ notification: Notification) {
    keyboardShown = true
    record()
  }
  @objc private func record() {
    resetIfRequested()
    if !initialFocus && window?.windowScene?.activationState == .foregroundActive {
      if secureFocus { _ = secure.becomeFirstResponder(); initialFocus = secure.isFirstResponder }
      else {
        _ = a.becomeFirstResponder(); initialFocus = a.isFirstResponder
        if initialFocus && ["middle", "ignore-once", "ignore-all", "selection-mutates", "selection-partial"].contains(activeSeed), let position = a.position(from: a.beginningOfDocument, offset: 7) {
          a.selectedTextRange = a.textRange(from: position, to: position)
        }
      }
    }
    func state(_ text: String) -> [String: Any] {
      ["characters": text.unicodeScalars.count,
       "sha256": SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined()]
    }
    func selection(_ field: UITextField) -> [String: Int] {
      guard let range = field.selectedTextRange else { return [:] }
      return ["location": field.offset(from: field.beginningOfDocument, to: range.start),
              "length": field.offset(from: range.start, to: range.end)]
    }
    let focus = a.isFirstResponder ? "a" : b.isFirstResponder ? "b" : secure.isFirstResponder ? "secure" : multi.isFirstResponder ? "multi" : "none"
    let body: [String: Any] = ["trial": trial, "title": titleLabel.text ?? "", "seed": activeSeed, "pid": ProcessInfo.processInfo.processIdentifier, "foreground_active": window?.windowScene?.activationState == .foregroundActive, "initial_focus_established": initialFocus, "keyboard_did_show": keyboardShown, "selection_commands": a.selectionCommands, "advance_on_clear": advanceOnClear, "advance_triggered": clearAdvance, "clear_fault":clearFault, "a_touch_starts":a.touchStarts, "a_touches_at_clear":touchesAtClear, "overlay_present":clearOverlay != nil, "overlay_actions":overlayActions, "a_enabled":a.isEnabled, "a_hidden":a.isHidden, "a_secure":a.isSecureTextEntry, "selection": ["a": selection(a), "b": selection(b)], "fields": ["a": state(a.text ?? ""), "b": state(b.text ?? ""), "secure": state(secure.text ?? ""), "multi": state(multi.text ?? "")], "focus": focus, "events": events, "shift_trigger_length": trigger, "a_touches_at_shift": touchesAtShift]
    guard let data = try? JSONSerialization.data(withJSONObject: body, options: [.sortedKeys]), data != last else { return }
    last = data
    let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    try? data.write(to: dir.appendingPathComponent("paired-state.json"), options: .atomic)
  }
  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    shiftFocus = ProcessInfo.processInfo.arguments.contains("--shift")
    secureFocus = ProcessInfo.processInfo.arguments.contains("--secure-focus")
    advanceOnClear = ProcessInfo.processInfo.arguments.contains("--advance-on-clear")
    b.text = advanceOnClear ? "keep-b" : ""
    let view = UIViewController()
    view.view.backgroundColor = .white
    let title = titleLabel; title.text = "Paired fixture " + trial; title.accessibilityIdentifier = "fixture-title"
    title.font = .systemFont(ofSize: 22, weight: .semibold)
    for (field, id) in [(a, "field-a"), (b, "field-b"), (secure, "field-secure")] {
      field.accessibilityIdentifier = id; field.accessibilityLabel = "Draft"
      field.borderStyle = .roundedRect; field.autocorrectionType = .no
      field.spellCheckingType = .no; field.autocapitalizationType = .none
      field.heightAnchor.constraint(equalToConstant: 48).isActive = true
      field.addTarget(self, action: #selector(changed(_:)), for: .editingChanged)
    }
    secure.isSecureTextEntry = true; secure.text = "benchmark-secure-dummy"
    multi.accessibilityIdentifier = "field-multiline"; multi.accessibilityLabel = "Multiline draft"
    multi.layer.borderWidth = 1; multi.heightAnchor.constraint(equalToConstant: 80).isActive = true
    activeSeed = option("--seed")
    a.selectionPolicy = activeSeed; a.selectionCommands = 0
    a.text = seededValue(activeSeed)
    let stack = UIStackView(arrangedSubviews: [title, a, b, multi, secure]); stack.axis = .vertical; stack.spacing = 20
    stack.translatesAutoresizingMaskIntoConstraints = false; view.view.addSubview(stack)
    NSLayoutConstraint.activate([stack.topAnchor.constraint(equalTo: view.view.safeAreaLayoutGuide.topAnchor, constant: 18), stack.leadingAnchor.constraint(equalTo: view.view.leadingAnchor, constant: 24), stack.trailingAnchor.constraint(equalTo: view.view.trailingAnchor, constant: -24)])
    NotificationCenter.default.addObserver(self, selector: #selector(keyboardDidShow(_:)), name: UIResponder.keyboardDidShowNotification, object: nil)
    window = UIWindow(windowScene: scene); window?.rootViewController = view; window?.makeKeyAndVisible()
    timer = Timer.scheduledTimer(timeInterval: 0.1, target: self, selector: #selector(record), userInfo: nil, repeats: true)
    DispatchQueue.main.async { [self] in
      record()
    }
  }
}
