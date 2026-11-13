// SPDX-License-Identifier: AGPL-3.0-or-later

declare module "*?raw" {
  const text: string;
  export default text;
}

interface ImportMeta {
  glob<T>(pattern: string, options: { query: "?raw"; import: "default"; eager: true }): Record<string, T>;
}
