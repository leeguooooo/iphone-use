import XCTest
@testable import IPhoneUseRemote

/// Where the remote's toast goes: never over the phone's picture.
final class ToastPlacementTests: XCTestCase {
    private let toast = CGSize(width: 150, height: 41)

    private func frame(_ placement: ToastGeometry.Placement?) -> CGRect? {
        switch placement {
        case .pill(let rect), .bar(let rect): rect
        case nil: nil
        }
    }

    /// Portrait iPhone: the picture fills the stage's height, so the toast
    /// goes on the key bar.
    func testPortraitWithNoRoomGoesOnTheKeyBar() {
        let stage = CGRect(x: 8, y: 120, width: 386, height: 646)
        let picture = CGRect(x: 52, y: 124, width: 298, height: 638)
        let keys = CGRect(x: 16, y: 778, width: 370, height: 62)
        let g = ToastGeometry(stage: stage, picture: picture, keys: keys,
                              top: CGRect(x: 16, y: 60, width: 370, height: 48), immersive: false)
        XCTAssertEqual(g.placement(for: toast), .bar(keys))
    }

    /// While typing there is no key bar: the top bar says it.
    func testTypingWithNoRoomGoesOnTheTopBar() {
        let stage = CGRect(x: 8, y: 120, width: 386, height: 400)
        let picture = CGRect(x: 105, y: 124, width: 192, height: 392)
        let top = CGRect(x: 16, y: 60, width: 370, height: 48)
        let g = ToastGeometry(stage: stage, picture: picture, keys: nil, top: top, immersive: false)
        XCTAssertEqual(g.placement(for: CGSize(width: 340, height: 41)), .bar(top))
    }

    /// Landscape: beside the picture, at the bottom of the band.
    func testLandscapeGoesBesideThePicture() throws {
        let stage = CGRect(x: 120, y: 8, width: 600, height: 386)
        let picture = CGRect(x: 331, y: 8, width: 178, height: 386)
        let g = ToastGeometry(stage: stage, picture: picture, keys: CGRect(x: 760, y: 80, width: 56, height: 260),
                              top: CGRect(x: 20, y: 20, width: 56, height: 300), immersive: false)
        let placed = try XCTUnwrap(frame(g.placement(for: toast)))
        guard case .pill = g.placement(for: toast) else { return XCTFail("expected a pill") }
        XCTAssertFalse(placed.intersects(picture))
        XCTAssertTrue(stage.contains(placed))
        XCTAssertGreaterThan(placed.minX, picture.maxX)
    }

    /// Immersive portrait: in the status bar's band above the picture,
    /// clear of the exit button in the corner.
    func testImmersiveGoesAboveThePicture() throws {
        let window = CGRect(x: 0, y: 0, width: 402, height: 874)
        let stage = CGRect(x: 0, y: 62, width: 402, height: 778)
        let picture = CGRect(x: 21, y: 62, width: 360, height: 778)
        var g = ToastGeometry(stage: stage, picture: picture, keys: nil, top: nil, immersive: true)
        g.window = window
        let placed = try XCTUnwrap(frame(g.placement(for: toast)))
        XCTAssertFalse(placed.intersects(picture))
        XCTAssertLessThan(placed.maxY, picture.minY)
        XCTAssertLessThan(placed.maxX, window.maxX - ToastGeometry.cornerButtonRoom)
    }

    /// A long toast too wide for the band beside the picture wraps to fit
    /// it rather than falling back to the narrow key rail.
    func testLongToastWrapsBesideThePicture() throws {
        let stage = CGRect(x: 130, y: 60, width: 615, height: 330)
        let picture = CGRect(x: 367, y: 60, width: 140, height: 330)
        let keys = CGRect(x: 790, y: 80, width: 56, height: 260)
        let g = ToastGeometry(stage: stage, picture: picture, keys: keys, top: nil, immersive: false)
        let wide = CGSize(width: 294, height: 41), wrapped = CGSize(width: 210, height: 62)
        guard case .pill(let placed) = g.placement(for: [wide, wrapped]) else { return XCTFail("expected a pill") }
        XCTAssertEqual(placed.size, wrapped)
        XCTAssertFalse(placed.intersects(picture))
    }

    /// Immersive with a banner over the picture's top: the toast stays in
    /// the status bar's band, above the safe area, off the banner.
    func testImmersiveStaysAboveTheSafeArea() throws {
        let window = CGRect(x: 0, y: 0, width: 402, height: 874)
        let content = CGRect(x: 0, y: 62, width: 402, height: 778)
        let picture = CGRect(x: 38, y: 128, width: 326, height: 712)
        var g = ToastGeometry(stage: CGRect(x: 0, y: 128, width: 402, height: 712), picture: picture,
                              keys: nil, top: nil, immersive: true, content: content)
        g.window = window
        let placed = try XCTUnwrap(frame(g.placement(for: CGSize(width: 260, height: 41))))
        XCTAssertLessThanOrEqual(placed.maxY, content.minY)
    }

    /// Room below the picture (iPad, a short picture) is the first choice.
    func testRoomBelowThePictureIsUsedFirst() throws {
        let stage = CGRect(x: 8, y: 80, width: 818, height: 1000)
        let picture = CGRect(x: 186, y: 80, width: 462, height: 900)
        let g = ToastGeometry(stage: stage, picture: picture, keys: CGRect(x: 157, y: 1100, width: 520, height: 62),
                              top: nil, immersive: false)
        let placed = try XCTUnwrap(frame(g.placement(for: toast)))
        XCTAssertGreaterThan(placed.minY, picture.maxY)
        XCTAssertLessThanOrEqual(placed.maxY, stage.maxY)
    }

    /// Zoomed in, the picture covers the stage: the key bar again.
    func testZoomedInPictureLeavesNoRoom() {
        let stage = CGRect(x: 0, y: 0, width: 900, height: 600)
        let keys = CGRect(x: 190, y: 620, width: 520, height: 62)
        let g = ToastGeometry(stage: stage, picture: stage, keys: keys, top: nil, immersive: false)
        XCTAssertEqual(g.placement(for: toast), .bar(keys))
    }
}
