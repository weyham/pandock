import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./style.css";

// 退役独立窗口之后只有一个窗口（设置页），不再按 label 分流。
createRoot(document.getElementById("root")!).render(<App />);
