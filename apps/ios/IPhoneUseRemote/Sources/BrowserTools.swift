import Foundation
import WebKit

struct BrowserPage: Codable, Sendable {
    let token: String
    let url: String
    let title: String
    let text: String
    let elements: [BrowserElement]
}

struct BrowserElement: Codable, Identifiable, Sendable {
    let id: Int
    let role: String
    let label: String
    let isSubmit: Bool
    let options: [String]
    let type: String
    let value: String?
}

enum BrowserToolError: LocalizedError {
    case invalidResult, stalePage, humanRequired, actionFailed(String)
    var errorDescription: String? {
        switch self {
        case .invalidResult: return String(localized: "无法读取这个页面，请重新加载后重试")
        case .stalePage: return String(localized: "页面已变化，需要重新读取")
        case .humanRequired: return String(localized: "这个输入需要你手动完成，完成后可以继续任务")
        case .actionFailed(let message): return message
        }
    }
}

@MainActor private let browserToolWorld = WKContentWorld.world(name: "PhoneUseTools")

extension BrowserSession {
    /// Numbered controls are valid only for this snapshot of this document.
    func readPage() async throws -> BrowserPage {
        let value = try await webView.callAsyncJavaScript(BrowserScripts.read, arguments: ["snapshotToken": UUID().uuidString],
            in: nil, contentWorld: browserToolWorld)
        guard let json = value as? String, let data = json.data(using: .utf8) else { throw BrowserToolError.invalidResult }
        return try JSONDecoder().decode(BrowserPage.self, from: data)
    }

    func click(element id: Int, token: String) async throws {
        _ = try await invoke(BrowserScripts.click,
            arguments: ["id": id, "token": token])
    }

    func fill(element id: Int, token: String, text: String) async throws {
        _ = try await invoke(BrowserScripts.fill,
            arguments: ["id": id, "token": token, "text": text, "personal": false])
        webView.endEditing(true)
    }
    func fillPersonal(element id: Int, token: String, text: String) async throws {
        _ = try await invoke(BrowserScripts.fill, arguments: ["id": id, "token": token, "text": text, "personal": true])
        webView.endEditing(true)
    }
    func select(element id: Int, token: String, value: String) async throws {
        _ = try await invoke(BrowserScripts.select, arguments: ["id": id, "token": token, "value": value])
    }

    func scroll(direction: String) async throws {
        guard ["up", "down"].contains(direction) else { throw BrowserToolError.actionFailed("滚动方向无效") }
        _ = try await webView.callAsyncJavaScript("window.scrollBy(0, innerHeight * 0.8 * direction); return true;",
            arguments: ["direction": direction == "down" ? 1 : -1], in: nil, contentWorld: browserToolWorld)
    }

    private func invoke(_ script: String, arguments: [String: Any]) async throws -> Any? {
        let wrapped = "try { " + script + " } catch (e) { return String(e.message || e); }"
        let result = try await webView.callAsyncJavaScript(wrapped, arguments: arguments, in: nil, contentWorld: browserToolWorld)
        if let message = result as? String {
            if message.hasPrefix("STALE:") { throw BrowserToolError.stalePage }
            if message.hasPrefix("HUMAN:") { throw BrowserToolError.humanRequired }
            throw BrowserToolError.actionFailed(message)
        }
        return result
    }

}

private enum BrowserScripts {
    static let read = """
    const nodes = [...document.querySelectorAll('a[href],button,input:not([type=hidden]),textarea,select,[role=button],[contenteditable=true]')]
      .filter(e => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < innerHeight && getComputedStyle(e).visibility !== 'hidden' && !e.closest('[aria-hidden=true]'); })
      .slice(0, 200);
    const token = snapshotToken;
    const sensitive = e => e.hasAttribute('data-phoneuse-sensitive') || ['password', 'file'].includes(e.type) || /^(cc-|one-time-code|current-password|new-password)/.test(e.getAttribute('autocomplete') || '');
    const signature = e => [e.tagName, e.type || '', e.getAttribute('href') || '', e.getAttribute('aria-label') || '', e.innerText || ''].join('|');
    globalThis.phoneUseSnapshot = { token, nodes, document, url: location.href, signatures: nodes.map(signature), signature };
    return JSON.stringify({ token, url: location.href, title: document.title,
      text: (document.body?.innerText || '').slice(0, 24000),
      elements: nodes.map((e,i) => ({ id: i+1, role: e.tagName.toLowerCase(), type: e.type || '', value: sensitive(e) ? null : ((e instanceof HTMLInputElement || e instanceof HTMLTextAreaElement || e instanceof HTMLSelectElement) ? e.value.slice(0, 2000) : null), options: e.tagName === 'SELECT' ? [...e.options].map(o => o.text).slice(0, 100) : [], isSubmit: !!e.form && (e.type === 'submit' || e.type === 'image'),
        label: (e.getAttribute('aria-label') || e.labels?.[0]?.innerText || e.innerText || e.getAttribute('placeholder') || e.getAttribute('title') || '').slice(0, 240) })) });
    """

    static let resolve = """
    const snapshot = globalThis.phoneUseSnapshot;
    if (!snapshot || snapshot.token !== token || snapshot.document !== document || snapshot.url !== location.href) throw new Error('STALE: Page changed. Read it again.');
    const element = snapshot.nodes[id-1];
    if (!element || !element.isConnected || !element.getClientRects().length || element.disabled) throw new Error('STALE: Control unavailable. Read the page again.');
    if (snapshot.signature(element) !== snapshot.signatures[id-1]) throw new Error('STALE: Control changed. Read the page again.');
    """

    static let click = resolve + """
    element.scrollIntoView({ block: 'center' });
    element.click();
    return true;
    """

    static let fill = resolve + """
    if (element.type === 'password' || element.type === 'file' || /^(cc-|one-time-code|current-password|new-password)/.test(element.getAttribute('autocomplete') || '')) throw new Error('HUMAN: Enter this field yourself.');
    element.focus();
    if (personal) element.setAttribute('data-phoneuse-sensitive', 'true');
    if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) {
      const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
      Object.getOwnPropertyDescriptor(prototype, 'value').set.call(element, text);
    } else if (element.isContentEditable) { element.textContent = text; }
    else { throw new Error('This control is not a text field.'); }
    element.dispatchEvent(new Event('input', { bubbles: true }));
    element.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
    """
    static let select = resolve + """
    if (!(element instanceof HTMLSelectElement)) throw new Error('This control is not a dropdown.');
    const option = [...element.options].find(o => o.text.trim() === value || o.value === value);
    if (!option || option.disabled) throw new Error('Option unavailable.');
    element.value = option.value;
    element.dispatchEvent(new Event('input', { bubbles: true }));
    element.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
    """

}
