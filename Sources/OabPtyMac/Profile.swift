import Foundation
import Security

/// A connection to an openab-pty runtime.
///
/// Two values are all a client needs, per the contract: everything else (session
/// names, attach tokens, stream offsets) is derived at runtime. The admin
/// credential makes this client the *operator*, which is what lets it renew its
/// own tokens instead of asking a human for one every time — the failure that
/// made browser dogfooding stall repeatedly.
struct Profile: Codable, Equatable {
    /// e.g. `http://192.168.0.25:8090`
    var baseURL: String
    var name: String

    var httpBase: URL? { URL(string: baseURL) }

    /// `ws://` for `http://`, `wss://` for `https://`.
    func webSocketURL(session: String, since: Int?) -> URL? {
        guard var comps = URLComponents(string: baseURL) else { return nil }
        comps.scheme = (comps.scheme == "https") ? "wss" : "ws"
        comps.path = "/pty/\(session)"
        if let since { comps.queryItems = [URLQueryItem(name: "since", value: String(since))] }
        return comps.url
    }
}

/// Keychain storage for the admin credential.
///
/// The whole reason a native client is worth building rather than a browser page:
/// the ADR forbids persisting a credential in a browser because there is nowhere
/// safe to put it. Here there is.
enum Keychain {
    private static let service = "dev.openab.oab-pty-mac"

    static func save(credential: String, for profileName: String) throws {
        let data = Data(credential.utf8)
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: profileName
        ]
        SecItemDelete(query as CFDictionary)
        query[kSecValueData as String] = data
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw NSError(domain: "Keychain", code: Int(status),
                          userInfo: [NSLocalizedDescriptionKey: "could not store credential (OSStatus \(status))"])
        }
    }

    static func credential(for profileName: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: profileName,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne
        ]
        var out: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func delete(profileName: String) {
        SecItemDelete([
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: profileName
        ] as CFDictionary)
    }
}

/// Profiles live in UserDefaults; only the credential goes to the Keychain.
enum ProfileStore {
    private static let key = "profiles"

    static func load() -> [Profile] {
        guard let data = UserDefaults.standard.data(forKey: key),
              let list = try? JSONDecoder().decode([Profile].self, from: data) else { return [] }
        return list
    }

    static func save(_ profiles: [Profile]) {
        if let data = try? JSONEncoder().encode(profiles) {
            UserDefaults.standard.set(data, forKey: key)
        }
    }
}
