import UIKit

struct ListRow {
  let identifier: String?
  let label: String
  let value: String
}

final class ListController: UIViewController, UICollectionViewDataSource, UICollectionViewDelegateFlowLayout {
  let mode: String
  let runID: String
  let model: [ListRow]
  var collection: UICollectionView!
  var counter = false
  var panEvents = 0
  var scrollEvents = 0
  var maxOffset = 0.0
  var maxAbsoluteOffset = 0.0
  var seen = Set<Int>()

  init(mode: String, runID: String) {
    self.mode = mode; self.runID = runID
    model = (0..<(mode == "counter" ? 8 : 32)).map { index in
      ListRow(identifier: mode == "missing" ? nil : (mode == "reused" ? "shared-row" : String(format: "row.%03d", index)),
              label: mode == "reused" ? String(format: "Item %03d", index) : "Same item",
              value: mode == "counter" ? "before" : "stable")
    }
    super.init(nibName: nil, bundle: nil)
  }
  required init?(coder: NSCoder) { fatalError("programmatic fixture only") }

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .white
    let layout = UICollectionViewFlowLayout()
    layout.minimumLineSpacing = 0; layout.minimumInteritemSpacing = 0
    collection = UICollectionView(frame: .zero, collectionViewLayout: layout)
    collection.translatesAutoresizingMaskIntoConstraints = false
    collection.backgroundColor = .white
    collection.showsVerticalScrollIndicator = false
    collection.allowsSelection = false
    collection.contentInsetAdjustmentBehavior = .never
    collection.dataSource = self; collection.delegate = self
    collection.register(UICollectionViewCell.self, forCellWithReuseIdentifier: "row")
    collection.register(UICollectionReusableView.self, forSupplementaryViewOfKind: UICollectionView.elementKindSectionFooter, withReuseIdentifier: "end")
    view.addSubview(collection)
    NSLayoutConstraint.activate([
      collection.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
      collection.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor),
      collection.leadingAnchor.constraint(equalTo: view.leadingAnchor),
      collection.trailingAnchor.constraint(equalTo: view.trailingAnchor)
    ])
    if mode == "counter" {
      collection.isScrollEnabled = false
      let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
      pan.cancelsTouchesInView = false; collection.addGestureRecognizer(pan)
    }
    NotificationCenter.default.addObserver(self, selector: #selector(activated(_:)), name: UIApplication.didBecomeActiveNotification, object: nil)
    NotificationCenter.default.addObserver(self, selector: #selector(activated(_:)), name: UIWindow.didBecomeKeyNotification, object: nil)
    saveOracle()
  }
  override func viewDidAppear(_ animated: Bool) {
    super.viewDidAppear(animated); collection.layoutIfNeeded(); saveState()
  }
  @objc func activated(_ notification: Notification) { saveState() }
  func collectionView(_ collectionView: UICollectionView, numberOfItemsInSection section: Int) -> Int { model.count }
  func collectionView(_ collectionView: UICollectionView, layout collectionViewLayout: UICollectionViewLayout, sizeForItemAt indexPath: IndexPath) -> CGSize {
    CGSize(width: collectionView.bounds.width, height: 60)
  }
  func collectionView(_ collectionView: UICollectionView, layout collectionViewLayout: UICollectionViewLayout, referenceSizeForFooterInSection section: Int) -> CGSize {
    CGSize(width: collectionView.bounds.width, height: 60)
  }
  func collectionView(_ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath) -> UICollectionViewCell {
    let cell = collectionView.dequeueReusableCell(withReuseIdentifier: "row", for: indexPath)
    let row = model[indexPath.item]
    cell.isAccessibilityElement = true
    cell.accessibilityIdentifier = row.identifier
    cell.accessibilityLabel = row.label
    cell.accessibilityValue = counter ? "after" : row.value
    let label: UILabel
    if let existing = cell.contentView.viewWithTag(100) as? UILabel { label = existing }
    else {
      label = UILabel(frame: .zero); label.tag = 100; label.isAccessibilityElement = false
      label.translatesAutoresizingMaskIntoConstraints = false; cell.contentView.addSubview(label)
      NSLayoutConstraint.activate([label.leadingAnchor.constraint(equalTo: cell.contentView.leadingAnchor, constant: 20), label.centerYAnchor.constraint(equalTo: cell.contentView.centerYAnchor)])
    }
    label.text = row.label
    cell.backgroundColor = indexPath.item % 2 == 0 ? .white : UIColor(white: 0.94, alpha: 1)
    return cell
  }
  func collectionView(_ collectionView: UICollectionView, viewForSupplementaryElementOfKind kind: String, at indexPath: IndexPath) -> UICollectionReusableView {
    let footer = collectionView.dequeueReusableSupplementaryView(ofKind: kind, withReuseIdentifier: "end", for: indexPath)
    footer.isAccessibilityElement = true
    footer.accessibilityLabel = "END OF LIST ORACLE"
    footer.accessibilityIdentifier = "oracle-end"
    return footer
  }
  func scrollViewDidScroll(_ scrollView: UIScrollView) { scrollEvents += 1; saveState() }
  func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) { saveState() }
  func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) { if !decelerate { saveState() } }
  @objc func panned(_ recognizer: UIPanGestureRecognizer) {
    if recognizer.state == .began {
      panEvents += 1; counter = true
      for cell in collection.visibleCells { cell.accessibilityValue = "after" }
      saveState()
    }
  }
  func write(_ object: [String: Any], name: String) {
    let url = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].appendingPathComponent(name)
    if let data = try? JSONSerialization.data(withJSONObject: object) { try? data.write(to: url, options: .atomic) }
  }
  func saveOracle() {
    write(["run_id": runID, "mode": mode, "rows": model.enumerated().map { index, row -> [String: Any] in
      ["index": index, "identifier": row.identifier.map { $0 as Any } ?? NSNull(), "label": row.label, "value": row.value]
    }], name: "list-oracle.json")
  }
  func saveState() {
    guard isViewLoaded, collection != nil else { return }
    let visible = collection.indexPathsForVisibleItems.map(\.item).sorted()
    seen.formUnion(visible); maxOffset = max(maxOffset, Double(collection.contentOffset.y))
    maxAbsoluteOffset = max(maxAbsoluteOffset, abs(Double(collection.contentOffset.y)))
    write(["run_id": runID, "mode": mode, "pid": ProcessInfo.processInfo.processIdentifier,
           "bundle_version": Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "",
           "width": view.bounds.width, "height": view.bounds.height,
           "application_state": UIApplication.shared.applicationState.rawValue,
           "scene_state": view.window?.windowScene?.activationState.rawValue ?? -1,
           "key_window": view.window?.isKeyWindow ?? false,
           "scroll_enabled": collection.isScrollEnabled, "interaction_enabled": collection.isUserInteractionEnabled,
           "content_height": collection.contentSize.height, "viewport_height": collection.bounds.height,
           "offset_y": collection.contentOffset.y, "max_offset_y": maxOffset, "max_abs_offset_y": maxAbsoluteOffset,
           "scroll_events": scrollEvents, "pan_events": panEvents, "counter_changed": counter,
           "visible_indices": visible, "seen_indices": seen.sorted()], name: "list-state.json")
  }
}
final class ListAppDelegate: UIResponder, UIApplicationDelegate {
  func application(_ application: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]?) -> Bool { true }
  func application(_ application: UIApplication, configurationForConnecting session: UISceneSession, options: UIScene.ConnectionOptions) -> UISceneConfiguration {
    let config = UISceneConfiguration(name: "List Oracle", sessionRole: session.role)
    config.delegateClass = ListSceneDelegate.self; return config
  }
}
final class ListSceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?
  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let args = ProcessInfo.processInfo.arguments
    func argument(_ name: String) -> String? { guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }; return args[i + 1] }
    guard let run = argument("--run-id"), UUID(uuidString: run) != nil,
          let mode = argument("--mode"), ["unique", "missing", "reused", "counter"].contains(mode) else { return }
    let window = UIWindow(windowScene: scene)
    window.rootViewController = ListController(mode: mode, runID: run)
    window.makeKeyAndVisible(); self.window = window
  }
}
UIApplicationMain(CommandLine.argc, CommandLine.unsafeArgv, nil, NSStringFromClass(ListAppDelegate.self))
