// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import KoidriveModel

extension AppModel {
    @MainActor
    static func preview(_ scenario: FakeScenario) -> AppModel {
        let model = AppModel.fake(scenario: scenario)
        model.start()
        return model
    }

    @MainActor
    static func preview(_ scenario: FakeScenario, telemetry: TelemetrySetting) -> AppModel {
        let model = AppModel.fake(scenario: scenario)
        model.session.fakeCore()?.setTelemetrySetting(setting: telemetry)
        model.start()
        return model
    }

    @MainActor
    static func previewWithOneSpace() -> AppModel {
        let model = AppModel.preview(.firstRun)
        _ = try? model.session.connect(url: "https://box.example.com")
        _ = try? model.session.claim(code: "R3VN-8KPT-Q2ZL", userName: "Gunwoo", deviceName: "Gunwoo's MacBook Pro")
        _ = try? model.session.createSpace(name: "Documents")
        return model
    }
}
