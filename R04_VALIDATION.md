# R04 驗證紀錄

日期：2026-10-09。基礎為使用者提供的 R02 Full Source (4)，與 (3) SHA-256 相同：
`873ba15b982633f244efba83eb0d952b475857410d91494e483a66d200d451ce`。
程式版本 0.5.1，自訂修訂 R04。未採用 R03 的新增功能。

| 檢查 | 結果 |
|---|---|
| `cargo fmt -- --check` | 通過 |
| `cargo test --locked -p babel-ebook` | 253 項通過，0 項失敗 |
| 核心與 CLI 的 Clippy，all-targets，warnings-as-errors | 通過 |
| 前端 TypeScript 檢查、正式建置 | 通過 |
| 4.1-mini 與 5.4-mini 的 HTTP 請求參數及 JSON Schema 路徑（本機 mock server） | 通過；各保留原有 Token／溫度參數 |
| 缺片段、無效 JSON、空片段的重試與拆組恢復 | 通過 |
| 格式與截斷恢復共用 9 次請求上限 | 通過 |
| 中斷／供應商錯誤不觸發格式恢復 | 通過 |
| 無效回覆不寫入成功快取、成功子組可於續譯沿用 | 通過 |
| 舊版 translation-v2 成功快取沿用 | 通過 |
| 序列化 R02 欄位的續譯紀錄：跳過完成章、翻譯失敗章 | 兩種模型設定均通過 |
| 提供的 Killer Clown EPUB：4.1-mini 嚴格合約、模擬格式失敗、正常及潤稿流程 | 39 個 spine 檔（含封面），78 次結構檢查通過 |
| 同一 EPUB：5.4-mini 的相同檢查 | 78 次通過；兩種模型合计 156 次 |

整本書檢查涵蓋 XML 解析、文字順序／完整性、段落／span／換行等節點數、ID、連結、
圖片引用、格式標記的局部還原。原始檔包含封面，因此 39 個 spine 檔與 GUI 的 38 個
可翻譯內容檔不同。

`checkpoint.rs`、全域設定、Translator 介面、桌面設定轉換與 app identifier 未變更；
cache scope 與 checkpoint translation signature 沿用 R02。這是相同模型、書檔及設定
可續譯的前提，不會讓不同模型共用快取。

## 驗證界線

- 以上是離線測試，未呼叫真實付費 API，未耗用使用者 credits。模擬回覆測試可驗證
  程式的請求與恢復流程，但不能替代真實模型譯文品質、拒答、限流或帳號權限的實測。
- 本地 `cargo clippy --workspace --all-targets` 與 `cargo test --workspace` 已嘗試，
  在 Linux 桌面依賴階段因缺少 pkg-config／GTK 系統依賴而停止；核心与 CLI 檢查已另行通過。
- 本地未產製 Windows EXE，也未執行 Windows WebView2 的 Playwright 測試。
  已將 R04 About 顯示的回歸檢查更新；此項須在 Windows 建置後執行。
- 更新包保留 Windows workflow，並加上格式、核心／CLI Clippy 閘門，核心測試、
  前端建置及桌面設定測試成功後才會產生安裝檔。請以成功的該次 workflow 產物安裝。
- 純註解修复演算法未改動；僅為既有較長的跨書籍函式加上區域 Clippy complexity
  註記，以維持既有行為並通過嚴格檢查。
