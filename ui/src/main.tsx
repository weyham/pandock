import React from "react";
import { createRoot } from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { App } from "./App";
import { ManagerApp } from "./NativeSync";
import { selectWindowMode } from "./windowMode";
import "./style.css";

const mode = selectWindowMode(getCurrentWindow().label);
createRoot(document.getElementById("root")!).render(mode === "native-sync-manager" ? <ManagerApp /> : <App />);
