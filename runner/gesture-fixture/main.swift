// Simulator-only touch receiver. Counters are reset only by a fresh launch.
import UIKit

final class TouchPad: UIView {
  private let result = UILabel()
  private let runID = ProcessInfo.processInfo.environment["GESTURE_RUN_ID"] ?? "missing"
  private var downs = 0, ups = 0, cancels = 0, maxFingers = 0
  private var start: CGPoint?, end: CGPoint?
  private var beganAt: TimeInterval?
  private var firstMotionAt: TimeInterval?
  private var edgeBeganAt: TimeInterval?
  private var hover: TimeInterval = 0
  private var elapsed: TimeInterval = 0
  private var visits: [String] = []
  private var touchStarts: [ObjectIdentifier: (CGPoint, TimeInterval)] = [:]
  private var observedHolds: [[String: Any]] = []

  override init(frame: CGRect) {
    super.init(frame: frame)
    backgroundColor = .systemBackground
    isMultipleTouchEnabled = true
    result.numberOfLines = 0
    result.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
    result.accessibilityIdentifier = "gesture-result"
    result.isUserInteractionEnabled = false
    addSubview(result)
  }
  required init?(coder: NSCoder) { fatalError("init(coder:) is unused") }

  override func layoutSubviews() {
    super.layoutSubviews()
    result.frame = CGRect(x: 16, y: safeAreaInsets.top + 12, width: bounds.width - 32, height: 190)
    publish()
  }
  override func draw(_ rect: CGRect) {
    super.draw(rect)
    for (x, y) in [(0.3, 0.4), (0.13, 0.5), (0.5, 0.4)] {
      let p = CGPoint(x: bounds.width * x, y: bounds.height * y)
      UIColor.systemBlue.setFill()
      UIBezierPath(ovalIn: CGRect(x: p.x - 5, y: p.y - 5, width: 10, height: 10)).fill()
    }
  }
  private func normalize(_ p: CGPoint) -> [Double] {
    [Double(p.x / max(bounds.width, 1)), Double(p.y / max(bounds.height, 1))]
  }
  private func sample(_ touch: UITouch, ending: Bool = false) {
    let p = touch.location(in: self)
    if firstMotionAt == nil, let start, hypot(p.x - start.x, p.y - start.y) > 2.5 {
      firstMotionAt = touch.timestamp
    }
    let edge = CGPoint(x: bounds.width * 0.13, y: bounds.height * 0.5)
    let inside = hypot(p.x - edge.x, p.y - edge.y) <= 2.5
    if inside {
      if edgeBeganAt == nil { edgeBeganAt = touch.timestamp }
      hover = max(hover, touch.timestamp - (edgeBeganAt ?? touch.timestamp))
    } else if let entered = edgeBeganAt {
      hover = max(hover, touch.timestamp - entered)
      edgeBeganAt = nil
    }
    for (name, x, y) in [("start", 0.3, 0.4), ("edge", 0.13, 0.5), ("end", 0.5, 0.4)] {
      if hypot(p.x - bounds.width * x, p.y - bounds.height * y) <= 4,
         visits.last != name { visits.append(name) }
    }
    if let beganAt { elapsed = max(elapsed, touch.timestamp - beganAt) }
    if ending { end = p; edgeBeganAt = nil }
  }
  private func publish() {
    let record: [String: Any] = [
      "run_id": runID, "down_count": downs, "up_count": ups, "holds": observedHolds,
      "cancel_count": cancels, "max_fingers": maxFingers,
      "hover_ms": Int(hover * 1000), "duration_ms": Int(elapsed * 1000),
      "initial_hold_ms": firstMotionAt.map { Int(($0 - (beganAt ?? $0)) * 1000) } as Any? ?? NSNull(),
      "start": start.map(normalize) as Any? ?? NSNull(),
      "end": end.map(normalize) as Any? ?? NSNull(),
      "visits": visits, "width": Double(bounds.width), "height": Double(bounds.height),
      "screen_width": Double(window?.screen.bounds.width ?? 0),
      "screen_height": Double(window?.screen.bounds.height ?? 0),
      "native_width": Double(window?.screen.nativeBounds.width ?? 0),
      "native_height": Double(window?.screen.nativeBounds.height ?? 0),
      "screen_scale": Double(window?.screen.scale ?? 0),
    ]
    if let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]),
       let text = String(data: data, encoding: .utf8) {
      result.text = text
      result.accessibilityLabel = text
    }
  }
  override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
    downs += touches.count
    maxFingers = max(maxFingers, event?.allTouches?.filter { ![.ended, .cancelled].contains($0.phase) }.count ?? touches.count)
    for touch in touches {
      touchStarts[ObjectIdentifier(touch)] = (touch.location(in: self), touch.timestamp)
      if beganAt == nil { beganAt = touch.timestamp; start = touch.location(in: self) }
      sample(touch)
    }
    publish()
  }
  override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
    for touch in touches {
      for sampleTouch in event?.coalescedTouches(for: touch) ?? [touch] { sample(sampleTouch) }
    }
    publish()
  }
  override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
    ups += touches.count
    for touch in touches {
      if let (origin, began) = touchStarts.removeValue(forKey: ObjectIdentifier(touch)) {
        observedHolds.append(["start": normalize(origin), "end": normalize(touch.location(in: self)),
                              "duration_ms": Int((touch.timestamp - began) * 1000)])
      }
      sample(touch, ending: true)
    }
    publish()
  }
  override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
    cancels += touches.count
    for touch in touches { sample(touch, ending: true) }
    publish()
  }
}
final class PadController: UIViewController {
  override func loadView() { view = TouchPad(frame: .zero) }
  override var prefersStatusBarHidden: Bool { true }
  override var prefersHomeIndicatorAutoHidden: Bool { true }
}

// Standard UIKit controls provide an independent receiver for input timing:
// action acknowledgements alone cannot prove a row selected or text arrived.
final class InputController: UIViewController, UITableViewDataSource, UITableViewDelegate, UISearchResultsUpdating, UISearchBarDelegate {
  private let result = UILabel()
  private let table = UITableView(frame: .zero, style: .plain)
  private let search = UISearchController(searchResultsController: nil)
  private let runID = ProcessInfo.processInfo.environment["GESTURE_RUN_ID"] ?? "missing"
  private var selections: [Int] = []
  private var edits: [String] = []
  private var beganEditing = 0

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .systemBackground
    title = "Input readiness"
    result.numberOfLines = 0
    result.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
    result.accessibilityIdentifier = "input-result"
    result.translatesAutoresizingMaskIntoConstraints = false
    table.translatesAutoresizingMaskIntoConstraints = false
    table.accessibilityIdentifier = "input-table"
    table.dataSource = self
    table.delegate = self
    table.rowHeight = 60
    view.addSubview(result)
    view.addSubview(table)
    NSLayoutConstraint.activate([
      result.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
      result.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 12),
      result.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -12),
      result.heightAnchor.constraint(equalToConstant: 130),
      table.topAnchor.constraint(equalTo: result.bottomAnchor),
      table.leadingAnchor.constraint(equalTo: view.leadingAnchor),
      table.trailingAnchor.constraint(equalTo: view.trailingAnchor),
      table.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor),
    ])
    search.obscuresBackgroundDuringPresentation = false
    search.searchResultsUpdater = self
    search.searchBar.delegate = self
    search.searchBar.placeholder = "Fixture search"
    navigationItem.searchController = search
    navigationItem.hidesSearchBarWhenScrolling = false
    definesPresentationContext = true
    publish()
  }
  func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { 80 }
  func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
    let cell = tableView.dequeueReusableCell(withIdentifier: "row") ?? UITableViewCell(style: .default, reuseIdentifier: "row")
    cell.textLabel?.text = "Fixture row \(indexPath.row)"
    cell.accessibilityIdentifier = "input-row-\(indexPath.row)"
    return cell
  }
  func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
    selections.append(indexPath.row)
    tableView.deselectRow(at: indexPath, animated: false)
    publish()
  }
  func scrollViewDidScroll(_ scrollView: UIScrollView) { publish() }
  func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) { publish() }
  func searchBarTextDidBeginEditing(_ searchBar: UISearchBar) { beganEditing += 1; publish() }
  func updateSearchResults(for searchController: UISearchController) {
    edits.append(searchController.searchBar.text ?? "")
    publish()
  }
  private func publish() {
    let record: [String: Any] = [
      "run_id": runID, "selections": selections, "text": search.searchBar.text ?? "",
      "edits": edits, "began_editing": beganEditing,
      "offset_y": Double(table.contentOffset.y),
      "decelerating": table.isDecelerating, "dragging": table.isDragging,
    ]
    if let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]),
       let text = String(data: data, encoding: .utf8) {
      result.text = text
      result.accessibilityLabel = text
    }
  }
}
@objc(GestureFixtureSceneDelegate)
final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?
  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let window = UIWindow(windowScene: scene)
    window.rootViewController = ProcessInfo.processInfo.environment["GESTURE_FIXTURE_MODE"] == "input"
      ? UINavigationController(rootViewController: InputController()) : PadController()
    window.makeKeyAndVisible()
    self.window = window
  }
}
final class AppDelegate: UIResponder, UIApplicationDelegate {}
UIApplicationMain(CommandLine.argc, CommandLine.unsafeArgv, nil, NSStringFromClass(AppDelegate.self))
