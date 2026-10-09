export type WindowMode = "settings" | "native-sync-manager";

export function selectWindowMode(label: string): WindowMode {
  return label === "native-sync-manager" ? "native-sync-manager" : "settings";
}
