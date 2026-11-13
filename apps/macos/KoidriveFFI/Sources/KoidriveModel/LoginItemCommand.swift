// SPDX-License-Identifier: Apache-2.0

import ServiceManagement

@MainActor
public enum LoginItemCommand {
    public static func unregister() throws {
        let service = SMAppService.mainApp
        if service.status == .enabled || service.status == .requiresApproval {
            try service.unregister()
        }
    }
}
