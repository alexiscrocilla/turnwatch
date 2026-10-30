; What the installer does beyond copying the app (`bundle.windows.nsis` in
; tauri.conf.json). This file keeps its UTF-8 BOM: without one, NSIS reads it
; in the system code page and garbles the accents.
;
; Updating runs the previous version's uninstaller first, without /UPDATE: it
; looks like a real uninstall. So "start with Windows" is remembered in the
; app's registry key, which Tauri deletes with the app data only. Windows' own
; off switch (StartupApproved) stays for the same reason.

!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define APPROVED_KEY "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
!define STARTED_WITH_WINDOWS "StartedWithWindows"

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Ajout du mod compagnon à Claude Desktop..."
  nsExec::ExecToStack /TIMEOUT=120000 '"$INSTDIR\${MAINBINARYNAME}.exe" --install-companion'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_ICONINFORMATION|MB_OK "Le mod compagnon n'a pas pu être ajouté à Claude Desktop. Le moniteur fonctionne sans lui, avec un quota moins précis.$\n$\nOuvre une fois l'onglet Code de Claude Desktop, puis relance cet installeur.$\n$\n$1" /SD IDOK
  ${EndIf}

  ReadRegStr $0 SHCTX "${MANUPRODUCTKEY}" "${STARTED_WITH_WINDOWS}"
  ${If} $0 == "1"
    WriteRegStr HKCU "${RUN_KEY}" "${BUNDLEID}" '"$INSTDIR\${MAINBINARYNAME}.exe"'
    DeleteRegValue SHCTX "${MANUPRODUCTKEY}" "${STARTED_WITH_WINDOWS}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Before anything is undone: this is where the user can still cancel.
  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ${If} $UpdateMode <> 1
    nsExec::ExecToStack /TIMEOUT=120000 '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-companion'
    Pop $0
    Pop $1

    ReadRegStr $0 HKCU "${RUN_KEY}" "${BUNDLEID}"
    ${If} $0 != ""
      WriteRegStr SHCTX "${MANUPRODUCTKEY}" "${STARTED_WITH_WINDOWS}" "1"
      DeleteRegValue HKCU "${RUN_KEY}" "${BUNDLEID}"
    ${EndIf}
    DeleteRegKey HKCU "Software\Classes\AppUserModelId\${BUNDLEID}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    DeleteRegValue HKCU "${APPROVED_KEY}" "${BUNDLEID}"
    ; What the mod wrote for the monitor.
    ReadEnvStr $0 "CLAUDE_CONFIG_DIR"
    ${If} $0 == ""
      StrCpy $0 "$PROFILE\.claude"
    ${EndIf}
    RMDir /r "$0\session-monitor"
  ${EndIf}
!macroend
