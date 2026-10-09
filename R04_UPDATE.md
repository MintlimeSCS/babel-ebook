# BabelEbook R04 更新與續譯步驟

本版直接以你提供的 R02 Full Source 為基礎，沒有採用已回退的 R03 修改。
自訂修訂名稱：**R04**；程式版本：**0.5.1**。

## 檔案用途

- `BabelEbook_R04_Modifications.zip`：只有本次新增／修改檔，適合覆蓋目前的 R02 原始碼。
- `BabelEbook_R04_Full_Source.zip`：完整 R04 原始碼，適合備份或建立乾淨的建置目錄。
- 兩者都不是 Windows 安裝檔。Windows `.exe` 由既有 GitHub Actions 流程建置。
- 完整包中的 `R04_VALIDATION.md` 說明已執行檢查與尚待 Windows 驗證的項目。

## 1. 保留目前翻譯進度

1. 先關閉 BabelEbook。
2. 備份目前實際使用的 `.babel_ebook_cache` 資料夾，以及設定中使用的續譯／checkpoint 資料夾。
   快取目前使用相對路徑，可能在程式啟動的工作目錄；請保留原位置，不能只備份輸出的 EPUB。
3. 保留原來的 `Killer_Clown_R02_Ready_EN.epub` 路徑、檔名與內容。
   本次上傳副本末尾的 `(1)` 不代表你電腦原始檔需要改名。
4. 記錄目前模型、V2.7 提示詞、輸入／輸出 Token、溫度、繁體中文與「僅譯文」等設定。

R04 保留 R02 的 translation-v2 快取與 checkpoint 簽章。相同書檔與設定可沿用已完成成果。
中途換模型、改提示詞或更改影響譯文的設定，會讓原快取／續譯紀錄不再符合；請先用原來的
`gpt-4.1-mini-2025-04-14` 完成這本書。

## 2. 更新 GitHub 原始碼

1. 在已確認為 R02 基礎的 `develop` 分支建立 `fix/r04-fragment-recovery` 分支。
   若目前分支名稱不同，先確認它確實是回退後的 R02，避免覆蓋到 R03 的其他修改。
2. 解壓 `BabelEbook_R04_Modifications.zip`。
3. 將 `R04_Modifications` 裡的資料夾與檔案，按相同相對路徑覆蓋到 repository 根目錄。
   請勿把 `R04_Modifications` 這一層資料夾本身放進 repository。
   `R04_PACKAGE_README.md` 和 `R04_FILES.json` 是包裝说明，無須作為程式檔上傳。
4. 特別確認 `.github/workflows/windows-manual.yml` 已更新。若網頁上傳沒有帶入 `.github`，
   可直接開啟該檔、按編輯、貼入修改包內相同檔案的完整內容。
5. 提交修改，建議訊息：`fix: R04 fragment recovery for GPT-4.1 and GPT-5.4`。

若以本機 Git 操作，覆蓋後先用 `git diff` 檢查，再 commit/push 到新分支。
本次交付不會自動上傳或改動你的 GitHub repository。

## 3. 建置 Windows 安裝檔

1. 開啟 GitHub repository 的 **Actions**。
2. 選擇 `windows-manual.yml` 對應的 **Build Windows installer** 流程。
   側欄名稱取決於預設分支，可能仍顯示 R02；重要的是下一步選擇已更新為 R04 的分支。
3. 按 **Run workflow**，選擇 R04 原始碼所在分支，啟動建置。
4. 等待整個流程成功。流程會做 Rust 格式／Clippy／核心測試、前端建置及桌面設定測試，
   然後產生 NSIS 安裝檔。
5. 從該次執行的 **Artifacts** 下載 `BabelEbook-Windows-R04`。
6. 解壓，取得 `BabelEbook_0.5.1_x64-setup.exe`。

## 4. 安裝並確認版本

1. 確認舊程式已關閉，再執行新安裝檔升級。
2. 本版保留原有 app identifier；不要清除快取、續譯紀錄或設定資料。
   若安裝程式要求移除舊版，請保留上述資料及備份。
3. 開啟程式，到 **About／關於** 確認顯示 **0.5.1 (R04)**。

## 5. 接續這本書

1. 選回電腦原位置的同一份 `Killer_Clown_R02_Ready_EN.epub`。
2. 確認模型仍是 `gpt-4.1-mini-2025-04-14`，其餘設定保持一致。
3. 在續譯區點選目前 **19／38 已完成、19 個失敗**、且顯示與來源匹配的那筆紀錄。
4. 按開始翻譯。已完成章節应跳過；失敗章節中的成功段落可命中原快取。
5. 若沒有顯示該筆紀錄，先確認原書檔、checkpoint 路徑與資料仍在，再確認設定。
   不要以清除快取或建立全新任務作為第一個排除手段。
6. 完成後查看是否 **38／38**，並確認沒有失敗紀錄。
   最終是否漏譯或錯譯仍需檢查產出的 EPUB；「翻譯成功」只代表處理完成。

## R04 的恢復與費用界線

- 正常段落維持一次請求；多片段回覆不合格時，先重試一次，再按原有片段邊界拆小。
- 格式恢復與輸出截斷恢復共用每個原始切片最多 **9 次邏輯翻譯嘗試**，不是每個片段各 9 次。
  既有暫時性 429 的 HTTP 重試另有原本上限。
- 輸出 Token 上限不會自動提高；恢復請求仍會產生正常 API 費用。
- 無效回覆不會當成成功保存；成功的小組另存快取，供下一次續譯沿用。
- 診斷檔存於 `.babel_ebook_cache/diagnostics-r04/`，只在本機保存失敗片段與回覆，
  不含 API Key 或完整系統提示詞。若再次失敗，可提供該次日誌與對應診斷檔查驗。
