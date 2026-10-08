# BabelEbook R02 UsageStats — 第一階段

本包為完整原始碼，基於本次重新上傳的 R02，不含 Windows EXE。

新增：OpenAI 實際 HTTP 翻譯呼叫、輸入／輸出 Token、API Cached Input Tokens、本地有效快取命中／未命中、HTTP 重試與截斷回復重試、模型綁定的估計 USD 費用。

在「設定 → 模型」填寫 USD／百萬 Token 的一般輸入、API 快取輸入、輸出單價。未設定時顯示未設定單價；換模型後舊單價不會套用新模型。V2.7 與既有提示詞檔未修改。

統計在日誌列持續更新；每次翻譯結束、失敗或正常取消，輸出 EPUB 同目錄另存獨立 `.usage-時間戳.json`。續譯只計本次新請求，不讀取以前的 usage。API 輸入已包含 cached input，費用依「一般輸入＝輸入−cached input」計算，避免重算。usage 缺失／取消未收回的回應不推估 Token；估價僅包含已回報用量，並非帳單。

快取統計：有效快取查找次數，無效資料視同未命中；完整段落查找後，若切片等於該段落，不重複查找同一鍵。長段落的完整鍵、切片鍵及回復子片為不同快取查找。

測試：前端 TypeScript／Vite 正式建置通過；10 項 usage 事件解析檢查通過；cargo fmt 語法解析／格式檢查通過。新增 3 個 Rust 用量測試，尚未執行：Cargo 套件下載被此環境網路政策阻擋（離線缺少 adler2 等依賴）。未執行 Windows EXE 建置、Windows WebView2 E2E 或真實付費 API。

Windows 建置：將 `BabelEbook_R02` 內檔案覆蓋到原專案根目錄（不要多包一層），保留 `.babel_ebook_cache`、checkpoints、settings 及 API Key。沿用 GitHub 分支 `fix/gpt5-completion-tokens` 操作，Actions → Build Windows installer R02 → Run workflow → 選您的修改分支。工作流程執行核心測試、前端建置、usage 解析測試及桌面設定測試，全部成功才建置。下載 BabelEbook-Windows-R02 artifact，解壓後執行 `*-setup.exe`；先關閉舊程式，再覆蓋安裝。「關於」顯示 R02 UsageStats，版本仍為 0.5.0。

本機建置：安裝 Rust 1.88+、Node 22、pnpm 9、Visual Studio Build Tools（Desktop development with C++）與 WebView2；在專案根 PowerShell 執行 `cargo test --locked -p babel-ebook`，進入 desktop 後 `pnpm install --frozen-lockfile`、`pnpm build`、`node scripts/test-usage.mjs`、`pnpm tauri build --bundles nsis`。安裝器在 `target\release\bundle\nsis`。
