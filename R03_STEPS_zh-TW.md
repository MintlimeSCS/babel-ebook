# BabelEbook R03 — 使用、建置與驗證說明

## 交付與來源

- `BabelEbook_R02_UsageStats.zip`：第一階段完整原始碼，只有新增用量統計。
- `BabelEbook_R03_Modified_Files.zip`：只含相對於原 R02 的新增／修改檔案，供直接覆蓋。
- `BabelEbook_R03_Full_Source.zip`：最終完整原始碼，含用量統計及相鄰短段落合併。要建置本次最終版本，使用此包即可。
- 基礎是本次 `BabelEbook_R02_Full_Source(3).zip`；已查得先前對話記錄，但未取得先前未交付修改的暫存檔。
- 各 ZIP 皆不是 Windows EXE。底層版本保持 0.5.0；最終「關於」顯示 `R03`。
- 包內 `MODIFIED_FILES_R03.txt` 是相對於本次原 ZIP 的完整修改清單；`CHANGES_R03.patch` 是可讀的逐檔差異。

## 新增功能及使用方式

### 用量／快取統計

在日誌列查看本次執行的統計；API 呼叫與回應到達時更新。每次成功、失敗或正常取消後，輸出 EPUB 同目錄會儲存 `<輸出檔名>.usage-<時間戳>.json`。強制終止程序或電腦斷電不保證留下最後報表。

|項目|定義|
|---|---|
|API 呼叫|OpenAI 翻譯 HTTP 實際發送嘗試；包括失敗和重試，不含連線測試／模型列表|
|輸入／輸出 Token|只累計 API 回傳 prompt_tokens／completion_tokens；不以字數估算|
|API Cached Input Tokens|API 回傳 cached_tokens，是輸入 Token 的一部分；與本機快取不同|
|本地命中／未命中|已驗證快取查找；空白／標記不符視為未命中。是查找次數，不是書中段落總數；批次失敗回退可能再次查找|
|HTTP 重試|暫時性網路／HTTP 錯誤的額外發送|
|回復重試|截斷拆片或合併結果不合格後的回復翻譯嘗試|
|用量未知|失敗、取消或未完整回傳 usage 的請求；其 Token 與費用不能從回應得知|
|估計費用|僅已回報 Token 按使用者設定單價計算，不是 OpenAI 實際帳單|

續譯的統計從零開始：恢復已完成章節不累加歷史 Token，快取讀取不增加 API Token。不同翻譯作業使用獨立統計，沒有全程式共用累計。

在「設定 → 模型」填入 USD／百萬 Token 的一般輸入、API 快取輸入、輸出單價。預設留空，不假設最新定價。單價綁定填入時的模型；換模型後不套用舊模型價格。一般輸入用 `輸入 Token − API Cached Input Token` 計價，避免 cached input 重複計費。若回應有 cached input 而未填該單價，費用顯示未設定。非 OpenAI 提供者保留原翻譯流程及本地快取統計，API 用量／估價不在本次新增範圍。

### 相鄰短段落合併

在「設定 → 模型」勾選「合併相鄰短段落」。預設關閉，可用同一設定比較開關效果。

建議初始參數：每段最多 **120 Token**，每組最多 **4 段**；容許範圍分別為 1–256、2–8。沿用先前建議的最大輸入 6,000／最大輸出 8,000，以及既有 V2.7。這是起始設定，仍以您實際書籍的完整性與回退頻率判斷效果。

只合併同一 XHTML 文件內，兄弟節點真正相鄰（中間只可有空白文字）的純文字 `<p>`。不跨標題、文件、不同容器、註解區、表格、列表、引用、圖片或其他節點。不合併有 id／語意屬性／可翻譯屬性或內嵌標籤的段落；腳註與連結走既有 R02 結構保護流程。啟用潤色／refine 時本次合併不啟用，保留原潤色流程。

合併只影響送給模型的請求，EPUB 中各原始段落仍獨立插入。每段有本地產生的識別碼，回應需驗證 JSON、段數、識別碼與順序、非空白、重複結果、殘留標記及明顯不完整內容。驗證後移除識別碼，不寫入 EPUB。無法用程序保證模型的語意翻譯百分之百完整；本次驗證處理可偵測的結構與明顯缺漏，仍需實際書籍抽查。

輸入預算包含有效提示詞、詞彙表、JSON、response schema 及安全空間；輸出保守保留約 3 倍原文 Token、識別碼和額外空間。不合預算就減少段數或使用原流程。輸出截斷、缺段、順序錯誤、識別碼錯誤或內容不合格，則回退逐段翻譯；已取消則停止，不啟動回退。所有段落驗證完成後才將合併結果逐段存入原快取鍵；再完成本地還原後才修改 DOM。不把不合格合併回應存入成功快取。

快取按原逐段鍵讀寫，合併開關與組數不改原快取識別；已快取段落不重送，也不跨越快取命中段落合併其他缺失段落。翻譯提示詞 V2.7 和提示詞檔不修改；只有合併請求追加程序所需的 JSON 回應協定，非改寫使用者翻譯規則。正常 p 段落書籍的 checkpoint 可繼續重用；含本次新增辨識文字區塊的書籍會重新檢查舊完成紀錄，防止沿用漏譯結果，已翻譯文字仍可重用本地快取。

### 原始 div 段落辨識修正

直接選原始 EPUB 即可，無需先逐本改檔。原文用 div 表示段落時，程式自動選取只有文字／行內元素的文字區塊，使用原有格式保護、快取及插入流程。原來的 div 標籤與章節／版面容器保留；不把整個 chapter 容器當成一段翻譯。巢狀 HTML 目錄中，未包含在其他翻譯段落內的文字超連結也會處理並保留 href。

純文字相鄰 div 段落可使用原合併開關、120 Tokens／4 段預設參數；有連結、id、內嵌格式、註解或標題的區塊保留逐段保護流程。同一合併組不跨越不同段落標籤。

本次只對含新辨識區塊的書籍加上續譯選取版本識別；舊漏譯的「已完成」章節會重查，新的完成紀錄之後正常續用。不需刪除快取。V2.7、GUI 選書流程及模型設定保持原樣。

加入原始 div 段落三種輸出模式、巢狀目錄、排除／正文／註解／圖說／表格範圍、合併／快取／標題邊界，以及升級後續譯的測試。格式檢查通過；這批新 Cargo 回歸測試在本機因缺少 adler2 等依賴未能執行，需由 GitHub Actions 驗證。尚未宣稱 Windows 新 EXE 或實際 OpenAI 全書翻譯已通過。

## 修改檔案

|檔案／群組|目的|
|---|---|
|crates/babel-ebook/src/usage.rs|本次統計、API usage 解析、費用計算、取消時未知用量|
|crates/babel-ebook/src/core.rs|每作業隔離、即時事件、獨立 JSON 報表|
|crates/babel-ebook/src/translator/http_common.rs|在真實 HTTP 發送與回應點記錄用量，含截斷回應|
|crates/babel-ebook/src/html/translation.rs|快取有效性統計、回復重試；公開既有片段協定給合併重用|
|crates/babel-ebook/src/html/mod.rs、merging.rs、merge_validation.rs|相鄰選取、Token 限制、逐段還原、結果驗證與安全回退|
|crates/babel-ebook/src/config.rs|新設定、舊設定預設值及範圍驗證|
|desktop/src/progress.ts、hooks/useLogState.ts|驗證統計事件並在 GUI 日誌更新|
|desktop/src/types.ts、config.ts、App.tsx、pages/ModelParamsPage.tsx|模型單價與合併設定、保存及參數傳遞|
|desktop/src-tauri/src/args.rs、config.rs|後端參數相容與轉換|
|desktop/src/pages/AboutPage.tsx|顯示 R03 識別|
|既有明列設定建構式的 Rust 測試檔|補上新增設定的預設值，保留原測試|
|tests/test_paragraph_merge.rs、usage.rs 內測試、desktop/scripts/test-usage.mjs|回退／順序／格式／快取／用量及解析測試|
|desktop/e2e/settings-navigation.spec.ts|Windows 設定操作回歸測試|
|.github/workflows/windows-manual.yml|安裝器建置前執行新增及原有測試|
|docs/README.md、CHANGELOG.md|記錄增量功能|

`cache.rs`、`epub.rs`、原有提示詞檔、V2.7、章節 checkpoint 簽章未改。完整清單以附帶清單為準。

## 已執行檢查與尚未驗證項目

|檢查|本次結果|
|---|---|
|前端 TypeScript + Vite 正式建置|第一階段及整合版皆通過|
|usage 事件解析測試|10 項通過|
|Rust 真實回應逐段驗證測試（生產用的 merge_validation.rs）|8 項通過，使用 rustc --test 實際編譯執行|
|cargo fmt 全專案語法解析／格式檢查|通過；僅表示可解析，不等同完整 Rust 型別與連結檢查|
|Rust 核心完整編譯／全套測試|未能完成；Cargo 依賴下載被環境網路政策阻擋，離線缺 adler2 等套件|
|新增 Rust EPUB／回退整合測試|已加入 6 個測試；因依賴限制尚未執行|
|Rust 用量／並行隔離測試|已加入 3 個測試；因依賴限制尚未執行|
|Windows 桌面後端、EXE／NSIS、WebView2 E2E|未在此 Linux 工作環境執行|
|真實付費 OpenAI API、Readest／Koodo 整本書驗證|未執行；未使用您的 API Key 或耗用額度|

Windows Actions 將在能取得依賴的 runner 上執行核心整合測試、前端建置、統計解析與桌面設定測試，通過後才產生安裝器。因此本包可供建置與查驗，但此環境尚未完成完整 Rust 編譯驗證，不能稱已驗證可安裝版。

## Windows 檔案替換、建置及安裝

### 沿用 GitHub Actions（與上次同一路徑）

1. 下載最終 ZIP，解壓後打開 `BabelEbook_R03`。其中的 Cargo.toml、crates、desktop、.github 等就是專案根內容；不要在原專案中再套一層 BabelEbook_R02。
2. 開啟您自己的 BabelEbook fork，切到先前使用的 `fix/gpt5-completion-tokens` 分支（或自己的修改分支）。以相同路徑替換所有清單中的檔案，包含新增檔及 `.github/workflows/windows-manual.yml`。GitHub「Upload files」時以資料夾內容上傳，注意 Windows 隱藏的 `.github` 不可漏掉。
3. 不刪除 `.babel_ebook_cache`、`.babel_ebook_checkpoints` 或您設定的 checkpoint 資料夾、設定檔與 API Key。ZIP 不含您的個人設定或 Key，也不要求重貼 V2.7。
4. 提交檔案後進入 **Actions → Build Windows installer R03 → Run workflow**，選擇剛提交的分支，再按 Run workflow。
5. 展開該次 run：先看 **Test paragraph response validation**、**Test translation core**、**Build desktop frontend**、**Test usage progress parser**、**Test desktop configuration** 是否成功。任何測試失敗就不要安裝；保留該步錯誤日誌供查修。
6. 整次 run 成功後，在頁面底部 Artifacts 下載 **BabelEbook-Windows-R03**，解壓，找到 `*-setup.exe`。
7. 完全關閉舊 BabelEbook；執行 setup，沿用原安裝位置覆蓋安裝。開啟程式，「關於」確認 **R03**。既有版本號仍為 0.5.0，不用版本號單獨判斷這次增量版。
8. 在「設定 → 模型」先查看費用欄，依實際模型填單價；合併先保留預設 120／4。使用同一原始書與設定開關合併，已有有效快取會重用，不能直接拿有快取與沒快取的 API 次數比較合併效果。若要公平比較，使用不同的暫存快取資料夾在 CLI 配置測試，或兩份獨立測試環境；不要為了比較刪除正式翻譯快取。
9. 先譯少量章節／測試文件，檢查日誌、JSON 用量、段落完整性、原譯交叉與註解連結，再開始整本。

### Windows 本機建置

先安裝 Rust 1.88 或更新的穩定版、Node.js 22、pnpm 9、Visual Studio Build Tools 的 Desktop development with C++、Windows SDK 與 WebView2。在完整原始碼專案根目錄開啟 PowerShell，逐行執行：

```powershell
cargo fmt --all -- --check
cargo test --locked -p babel-ebook
cargo test --locked -p babel-ebook-desktop --lib config::tests
cd desktop
pnpm install --frozen-lockfile
pnpm build
node scripts/test-usage.mjs
pnpm tauri build --bundles nsis
```

Windows NSIS 安裝器位於專案根下 `target\release\bundle\nsis\*-setup.exe`。步驟任一失敗先停止查修，不要把未建置的原始碼 ZIP 當成安裝器。命令不使用您的 API Key；核心 mock 測試不耗付費 API。
