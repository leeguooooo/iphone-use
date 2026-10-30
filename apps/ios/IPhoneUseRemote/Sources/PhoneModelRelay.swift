import Foundation
@preconcurrency import Network

/// Model HTTP stays on this phone. URLSession performs system DNS/TLS on behalf
/// of the native runner, whose process cannot resolve external hostnames.
@MainActor
final class PhoneModelRelay {
    static let shared = PhoneModelRelay()
    private var listener: NWListener?
    private var port: UInt16?
    private var waiters: [CheckedContinuation<Void, Error>] = []
    private var routes: [String: ModelsPayload.Assist] = [:]
    private var clients: [UUID: NWConnection] = [:]
    private var taskRoutes: [String: [String]] = [:]
    private let queue = DispatchQueue(label: "iphone-use-model-relay")
    private let session = URLSession(configuration: .ephemeral, delegate: BrowserModelRedirectGuard(), delegateQueue: nil)

    func payload(_ input: ModelsPayload?) async throws -> ModelsPayload? {
        guard var value = input, !value.assist.isEmpty else { return input }
        try await start()
        guard let port else { throw BrowserToolError.actionFailed("手机模型转发服务未就绪") }
        value.assist = value.assist.map { provider in
            let id = UUID().uuidString.lowercased()
            routes[id] = provider
            var local = provider
            local.baseURL = "http://127.0.0.1:\(port)/\(id)"
            return local
        }
        return value
    }

    func bind(_ payload: ModelsPayload?, task: String) {
        taskRoutes[task] = payload?.assist.compactMap { URL(string: $0.baseURL)?.path.split(separator: "/").first.map(String.init) } ?? []
    }
    func release(task: String) {
        for id in taskRoutes.removeValue(forKey: task) ?? [] { routes.removeValue(forKey: id) }
    }

    private func start() async throws {
        if port != nil { return }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            waiters.append(continuation)
            guard listener == nil else { return }
            do {
                let parameters = NWParameters.tcp
                parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
                let listener = try NWListener(using: parameters, on: .any)
                self.listener = listener
                listener.stateUpdateHandler = { [weak self] state in
                    Task { @MainActor in
                        guard let self else { return }
                        switch state {
                        case .ready:
                            self.port = listener.port?.rawValue
                            let pending = self.waiters; self.waiters = []
                            pending.forEach { $0.resume() }
                        case .failed(let error):
                            self.listener = nil
                            let pending = self.waiters; self.waiters = []
                            pending.forEach { $0.resume(throwing: error) }
                        default: break
                        }
                    }
                }
                listener.newConnectionHandler = { [weak self] connection in
                    Task { @MainActor in self?.accept(connection) }
                }
                listener.start(queue: queue)
            } catch {
                let pending = waiters; waiters = []
                pending.forEach { $0.resume(throwing: error) }
            }
        }
    }

    private func accept(_ connection: NWConnection) {
        guard clients.count < 8 else { connection.cancel(); return }
        let id = UUID(); clients[id] = connection
        connection.start(queue: queue)
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(65))
            if clients[id] != nil { close(id) }
        }
        read(connection, id: id, buffer: Data())
    }

    private func read(_ connection: NWConnection, id: UUID, buffer: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, ended, error in
            Task { @MainActor in
                guard let self, self.clients[id] != nil else { return }
                var buffer = buffer; if let data { buffer.append(data) }
                guard error == nil, buffer.count <= 4_194_304 else { self.close(id); return }
                if let split = buffer.range(of: Data("\r\n\r\n".utf8)) {
                    guard split.lowerBound <= 16_384,
                          let head = String(data: buffer[..<split.lowerBound], encoding: .utf8) else { self.close(id); return }
                    let lines = head.components(separatedBy: "\r\n")
                    let first = (lines.first ?? "").split(separator: " ")
                    guard first.count == 3 else { self.close(id); return }
                    var headers: [String: String] = [:]
                    for line in lines.dropFirst() {
                        guard let colon = line.firstIndex(of: ":") else { self.close(id); return }
                        let key = line[..<colon].lowercased()
                        guard headers[key] == nil else { self.close(id); return }
                        headers[key] = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
                    }
                    guard headers["transfer-encoding"] == nil,
                          let count = Int(headers["content-length"] ?? "0"), count >= 0, count <= 4_000_000 else { self.close(id); return }
                    let body = Data(buffer[split.upperBound...])
                    if body.count == count {
                        await self.forward(method: String(first[0]), path: String(first[1]), headers: headers, body: body, id: id)
                        return
                    }
                    guard body.count < count, !ended else { self.close(id); return }
                } else if buffer.count > 16_384 || ended { self.close(id); return }
                self.read(connection, id: id, buffer: buffer)
            }
        }
    }

    private func forward(method: String, path: String, headers: [String: String], body: Data, id: UUID) async {
        let parts = path.split(separator: "/", omittingEmptySubsequences: true)
        guard parts.count >= 2, let provider = routes[String(parts[0])] else { close(id); return }
        let suffix = parts.dropFirst().joined(separator: "/")
        guard (method == "GET" && suffix == "models") ||
              (method == "POST" && ["chat/completions", "messages", "v1/messages"].contains(suffix)) else { close(id); return }
        let credential = provider.format == "anthropic" ? headers["x-api-key"] : headers["authorization"]
        let expected = provider.format == "anthropic" ? provider.key : "Bearer " + provider.key
        guard provider.key.isEmpty || credential == expected else { close(id); return }
        let base = provider.baseURL.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        let upstreamSuffix = provider.format == "anthropic" && base.hasSuffix("/v1") && suffix == "v1/messages" ? "messages" : suffix
        guard let url = URL(string: base + "/" + upstreamSuffix), url.scheme == "https", url.user == nil, url.password == nil,
              url.query == nil, url.fragment == nil else { close(id); return }
        var request = URLRequest(url: url); request.httpMethod = method
        request.timeoutInterval = 55
        if method == "POST" { request.httpBody = body; request.setValue("application/json", forHTTPHeaderField: "Content-Type") }
        request.setValue("iphone-use/1.0", forHTTPHeaderField: "User-Agent")
        for name in ["authorization", "x-api-key", "anthropic-version"] {
            if let value = headers[name] { request.setValue(value, forHTTPHeaderField: name) }
        }
        do {
            var (data, response) = try await session.data(for: request)
            // Same fallback as the app's own requests: a model that refuses a
            // forced tool_choice gets one retry with "auto".
            if method == "POST", (response as? HTTPURLResponse)?.statusCode == 400,
               String(decoding: data, as: UTF8.self).contains("tool_choice"),
               var object = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any], object["tool_choice"] != nil {
                object["tool_choice"] = provider.format == "anthropic" ? ["type": "auto"] as [String: Any] : "auto"
                request.httpBody = try JSONSerialization.data(withJSONObject: object)
                (data, response) = try await session.data(for: request)
            }
            guard let response = response as? HTTPURLResponse, data.count <= 4_194_304 else { throw BrowserToolError.invalidResult }
            respond(id, status: response.statusCode, data: data)
        } catch {
            respond(id, status: 502, data: Data("{\"error\":\"phone_model_network_unavailable\"}".utf8))
        }
    }

    private func respond(_ id: UUID, status: Int, data: Data) {
        guard let connection = clients[id] else { return }
        var response = Data("HTTP/1.1 \(status) Result\r\nContent-Type: application/json\r\nContent-Length: \(data.count)\r\nConnection: close\r\n\r\n".utf8)
        response.append(data)
        connection.send(content: response, completion: .contentProcessed { [weak self] _ in
            Task { @MainActor in self?.close(id) }
        })
    }
    private func close(_ id: UUID) { clients.removeValue(forKey: id)?.cancel() }
}
