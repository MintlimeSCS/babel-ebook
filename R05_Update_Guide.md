# BabelEbook R05 更新與 40／41 續譯指南

自訂版本：**R05**；程式版本：**0.5.2**。
本版以 R04 Full Source 為基礎，並納入先前已交付的 R04 GitHub 建置修正（固定 Rust 1.90.0），沒有採用已回退的 R03 功能。

## 檔案用途

| 檔案 | 用途 |
|---|---|
| BabelEbook_R05_Modifications.zip | 覆蓋目前 R04 repository 的新增／修改檔案 |
| BabelEbook_R05_Full_Source.zip | 完整 R05 原始碼，供乾淨建置或備份 |
| 9b6302f7c836c267.json（已另外交付） | 本書的 40／41 恢復紀錄；不是程式更新檔 |

前兩個 ZIP 都不是 Windows 安裝程式。安裝 EXE 由 GitHub Actions 建置。

## 一、保留目前成果

1. 關閉 BabelEbook。
2. 備份設定所指定的 checkpoints 資料夾、實際使用的 `.babel_ebook_cache` 資料夾及原有輸出 EPUB。
3. 將已交付的 **40／41 恢復檔**放回 checkpoints，檔名必須為 **`9b6302f7c836c267.json`**，移除下載附加的 `(1)`／`(2)`。先備份原有同名檔，再覆蓋。
4. 保留原始英文 EPUB。恢復 JSON 內已有 40 個完成項目的 HTML；第 12 章仍需由英文原書續譯。

## 二、更新 GitHub 程式碼

1. 在已確認為 R04 的 `develop` 基礎建立 `fix/r05-resume-diagnostics` 分支；若 R04 位於其他工作分支，先確認該分支內容正確。
2. 解壓 `BabelEbook_R05_Modifications.zip`。
3. 將 **`R05_Modifications` 內部**的檔案／資料夾，依相同相對路徑覆蓋至 repository 根目錄。不要把 `R05_Modifications` 這一層一起上傳。
4. `.github/workflows/windows-manual.yml` 必須更新。若瀏覽器上傳沒有包含隱藏的 `.github` 資料夾，請在 GitHub 開啟該檔，貼上 ZIP 中該檔的完整內容。
5. 提交訊息可用 `fix: R05 resume protection and truncation diagnostics`。
6. ZIP 根目錄的 `R05_FILES.json` 與 `R05_PACKAGE_README.md` 是封裝說明，不是程式檔，無須一併上傳。

## 三、建置及安裝

1. 開啟 repository 的 **Actions**。
2. 選擇 `windows-manual.yml` 對應的 **Build Windows installer** 流程。側欄可能仍顯示預設分支的 R04 名稱；按 **Run workflow** 時務必選擇已更新為 R05 的分支。
3. 等待格式、核心／CLI Clippy、核心測試、前端建置及桌面設定測試全部成功。
4. 從該次執行的 **Artifacts** 下載 **`BabelEbook-Windows-R05`**。
5. 解壓，執行 **`BabelEbook_0.5.2_x64-setup.exe`** 升級。R05 保持原 app identifier；不要清除設定、快取或 checkpoints。
6. 在 About／關於確認 **0.5.2 (R05)**。

## 四、首次使用本書恢復紀錄

1. 選取原始英文 EPUB。
2. 選取顯示 **40／41 完成、1 個失敗**的恢復紀錄。
3. **Input Token 改回使用者確認的原值：6000；Output Token 維持原值：8000。** 模型、提示詞、溫度、繁體中文、僅譯文等其餘設定也保持原來的值。
4. 按開始翻譯。R05 先核對原書與舊版完整設定簽章；匹配後，備份原進度並保存 R05 相容性欄位，再排程第 12 章。其餘 40 個完成項目從 JSON 還原，完成章節的目錄名稱優先在本機取回。
5. 若模型／提示詞／Token 設定不相容，會停止並保留原進度，不會清空為 0／41。這時應恢復原設定，而不是刪除紀錄重新翻譯。

首次設定驗證並保存之後，即使第 12 章又失敗，紀錄仍已遷移為 R05；之後可調整 Input／Output Token 而不重置已完成章節。

**可調整 Token 不代表任意數值都可用。** Input 必須容納提示詞和原文。若提示詞本身很長，2000 可能過小；R05 會在付費請求前報出估計的最低需求並停止，保留 40／41。請依該訊息調整，不要反覆盲目降低。

## 五、若第 12 章再次達到 Output 上限

1. 40 個完成項目會保留，只更新第 12 章的失敗狀態。
2. 日誌末尾新增 `diagnostic=...`，提供診斷 JSON 的**完整本機路徑**。
3. 診斷檔位於實際快取目錄的 **`diagnostics-r05`** 子資料夾。提供該次對應檔案及日誌即可分析。
4. 檔案記錄實際送出片段、截斷回應、Token 用量、模型、request ID、格式合約及恢復深度／位置；很長的內容保存頭尾並標示省略字數。API Key、授權標頭及完整系統提示詞不會寫入。
5. 既有格式驗證錯誤仍使用 `diagnostics-r04`；請優先依本次日誌指定的位置取檔。

只保存已收到的失敗回應，**沒有額外 API 呼叫或 credits 費用**。上限為 50 份／10 MiB，正常成功請求不保存全文。

R05 不會自動提高 Output Token，也不增加既有恢復重試上限；因此本版不保證該章一次成功。新紀錄用於判斷是否重複輸出、異常延伸、片段／格式合約問題，之後再作有根據的修正。

## 六、其他保護及限制

- 相容續譯前會在 checkpoints 的 `backups` 子資料夾留存原紀錄，每個任務保留最近 3 份；這些備份不會出現在正常續譯清單中。
- 改模型、提示詞、詞彙表、語言、溫度、潤稿或輸出範圍仍視為不同翻譯設定；如要重新翻譯，應使用獨立的 checkpoints 資料夾建立新任務。
- R02／R04 成功段落快取可在原設定首次命中後轉存至 R05，往後調整 Token 可沿用。若舊快取已遺失，失敗章節內的部分成功段落可能重送；40 個完成項目不受影響。
- 已完成章節的目錄名稱在本機優先從譯文標題還原；沒有可用譯文標題時保留原目錄名稱，不為完成章節另外付費翻譯標題。
- 本次未修改你的 GitHub，也未呼叫真實付費 API。Windows 安裝／WebView2 測試由 Actions 與實機完成。
