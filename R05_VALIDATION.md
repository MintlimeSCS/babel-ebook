# R05 驗證紀錄

日期：2026-10-12。自訂修訂 **R05**；程式版本 **0.5.2**。
基礎：R04 Full Source，加上已交付的 R04 GitHub Build Fix（Rust 1.90.0）。未增加任何第三方依賴，Cargo.lock 只更新本專案三個套件的版本號。

## 已通過的檢查

| 檢查 | 結果 |
|---|---|
| Rust 1.90.0 `cargo fmt -- --check` | 通過 |
| `git diff --check` | 通過 |
| `cargo test --locked -p babel-ebook` | **259 passed、0 failed、2 ignored** |
| `cargo clippy --locked -p babel-ebook -p babel-ebook-cli --all-targets -- -D warnings` | 通過 |
| 前端 `pnpm exec tsc --noEmit` | 通過 |
| 前端 `pnpm build` | 通過 |
| 原書＋已交付 40／41 JSON 的指定離線恢復測試 | **1 passed**；只嘗試第 12 章，兩次模擬失敗後其餘 40 項的 HTML 位元組不變 |

2 個預設 ignored 項目中，一個為原有可選測試；另一個為需要使用者檔案的 R05 測試，已另行指定原書及恢復 JSON 執行並通過。原書、JSON 均不嵌入程式 ZIP。

## 本次關鍵回歸

- 第一次以相同原設定載入 R02／R04 JSON，加入 R05 相容性欄位；在付費章節請求前保存遷移結果。
- 40 完成／1 失敗的任務連續失敗四次、其間改 Input 6000→4000→2000→3000（使用短模擬提示詞），完成內容逐位元組保留，每次只有失敗章節呼叫模擬翻譯器一次。
- 原書內容、提示詞、詞彙表、潤稿設定不相容，或舊版 Token 設定不匹配，立即停止；checkpoint 原檔不變。
- JSON 損壞或 Input 不足以容納長提示詞，停止、保留原檔，模擬翻譯器零次呼叫。
- 每次相容續譯備份舊紀錄，保留每個任務最近三份備份。
- R04 既有成功段落快取在相同設定命中後遷移至 R05，再調整 Token 仍沿用，不新增翻譯呼叫。
- GPT-4.1-mini 與 GPT-5.4-mini 各用本機 mock HTTP 回傳 length／8000 tokens：各僅一次原請求，保存原始失敗回應、位置、Token 用量及 request ID；維持兩種模型的原 Token 參數格式，不保存 API Key 或系統提示詞全文。
- 既有小片段恢復、格式合約、截斷保護、9 次上限、API 參數、EPUB 格式及註解回歸仍通過。
- UTF-8 頭尾節錄安全；診斷保留數量受控，50 份／10 MiB 清理程式生效。

## 實際檔案測試的界線

本機測試使用真實原書章節結構和真實恢復 JSON 的 40 個譯文項目，但翻譯器為刻意失敗的離線模擬器。
為隔離付費模型，此測試只在暫存副本中改寫設定簽章為模擬器簽章；原始恢復 JSON 和原書檔沒有修改。
因此，它驗證的是解析、章節對應、內容保留及 Token 調整後的恢復流程，不能代替使用者在 R05 的第一次真實原設定簽章核對，也不能證明第 12 章模型回應必定成功。
首次實機續譯依使用者確認應用 **Input 6000、Output 8000** 及原模型／提示詞。

## 桌面／Windows 驗證界線

已嘗試完整 workspace 測試／Clippy，以及桌面設定測試。Linux 容器的原生桌面依賴階段缺少 pkg-config／GTK，無法完成 Tauri 桌面編譯；不把這些檢查列為通過。
本次沒有產製 Windows EXE，也沒有执行 Windows WebView2 Playwright。對應 E2E 已更新 R05 顯示及舊紀錄提示斷言。
Windows manual workflow 保留固定 Rust 1.90.0、核心及 CLI Clippy、完整核心測試、前端建置、桌面設定測試與 NSIS 安裝檔建置；使用成功的 `BabelEbook-Windows-R05` artifact 安裝。

## 費用與交付

- **沒有呼叫真實付費翻譯 API，沒有耗用使用者 API credits。**
- 新增診斷只保存已收到的截斷回應，不會為記錄再發一次請求。
- 模型 Output 上限和原有恢復預算不增加；未聲稱已解決本書第 12 章的模型異常生成。
- 未對使用者 GitHub 上傳、提交或部署。
- ZIP 封装包含相對路徑與 SHA-256 清單；封裝後驗證 CRC、完整原始碼與修改包內容一致。
