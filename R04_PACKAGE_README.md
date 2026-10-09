# R04 修改包

直接以 R02 為基礎。本資料夾中的程式資料夾／檔案與 repository 根目錄一一對應。
將它們按相同相對路徑覆蓋到 R02 原始碼；不要把 R04_Modifications 這一層放入 repository。
本包無須刪除既有程式檔。R04_PACKAGE_README.md、R04_FILES.json 是包裝說明與核對表。

完整更新、建置、安裝、續譯步驟：R04_UPDATE.md。
已完成測試與限制：R04_VALIDATION.md。
GitHub Actions 成功後下載 BabelEbook-Windows-R04 artifact，執行 0.5.1 安裝檔。
安裝前保留既有快取、checkpoint 與同一份來源 EPUB；About 應顯示 0.5.1 (R04)。
