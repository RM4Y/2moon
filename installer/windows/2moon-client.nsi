; 2moon client for Windows: installer (NSIS, built by scripts/build-windows-installer.sh).
;
; Per-user install, no administrator rights: moon-client.exe and its DLLs in
; %LOCALAPPDATA%\Programs\2moon, a Start menu shortcut (and a desktop one, optional),
; an entry in Settings > Apps for the current user. Settings and the client
; identity stay in %APPDATA%\2moon-client (written by the client itself).
; The portable zip (scripts/package-release.sh) is still built next to it.
;
; Silent install / uninstall:
;   2moon-client-<version>-windows-x86_64-setup.exe /S [/D=C:\path]
;   "%LOCALAPPDATA%\Programs\2moon\uninstall.exe" /S   (keeps %APPDATA%\2moon-client)

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef STAGE
  !define STAGE "stage-client"
!endif
!ifndef OUTFILE
  !define OUTFILE "2moon-client-setup.exe"
!endif

!define APPNAME "2moon"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\2moon-client"
!define EXE "moon-client.exe"

Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\2moon"
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"
BrandingText "2moon ${VERSION}"

!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

!define MUI_ICON "2moon.ico"
!define MUI_UNICON "2moon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "$(FINISH_RUN)"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
; Follows the Windows display language; English is the fallback for the others.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "French"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductName" "${APPNAME}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileDescription" "2moon client installer"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "LegalCopyright" "GPL-3.0-or-later"

; ------------------------------------------------------------------ strings

LangString MSG_NEED64 ${LANG_ENGLISH} "2moon requires 64-bit Windows."
LangString MSG_NEED64 ${LANG_FRENCH} "2moon nécessite Windows 64 bits."
LangString MSG_CLOSE_APP ${LANG_ENGLISH} "2moon is running. Close it, then click Retry."
LangString MSG_CLOSE_APP ${LANG_FRENCH} "2moon est ouvert. Fermez-le, puis cliquez sur Réessayer."
LangString FINISH_RUN ${LANG_ENGLISH} "Launch 2moon"
LangString FINISH_RUN ${LANG_FRENCH} "Lancer 2moon"
LangString NAME_SecApp ${LANG_ENGLISH} "2moon client"
LangString NAME_SecApp ${LANG_FRENCH} "Client 2moon"
LangString NAME_SecDesktop ${LANG_ENGLISH} "Desktop shortcut"
LangString NAME_SecDesktop ${LANG_FRENCH} "Raccourci sur le Bureau"
LangString DESC_SecApp ${LANG_ENGLISH} "The streaming client and a Start menu shortcut. Installed for your account only, no administrator rights needed."
LangString DESC_SecApp ${LANG_FRENCH} "Le client de streaming et son raccourci dans le menu Démarrer. Installé pour votre compte seulement, sans droits d'administrateur."
LangString DESC_SecDesktop ${LANG_ENGLISH} "A 2moon shortcut on the desktop."
LangString DESC_SecDesktop ${LANG_FRENCH} "Un raccourci 2moon sur le Bureau."
LangString UN_ASK_DATA ${LANG_ENGLISH} "Also delete the 2moon settings (saved hosts, pairing identity)?"
LangString UN_ASK_DATA ${LANG_FRENCH} "Supprimer aussi les réglages de 2moon (hôtes enregistrés, identité d'appairage) ?"

; ------------------------------------------------------------------ helpers

; A running client keeps its .exe and DLLs locked: ask to close it (silent mode:
; close it, it is the user's own process so no rights are needed).
!macro WAIT_CLIENT_CLOSED un
Function ${un}WaitClientClosed
  ${IfNot} ${FileExists} "$INSTDIR\${EXE}"
    Return
  ${EndIf}
  ${Do}
    ClearErrors
    FileOpen $R0 "$INSTDIR\${EXE}" a
    ${IfNot} ${Errors}
      FileClose $R0
      Return
    ${EndIf}
    ${If} ${Silent}
      nsExec::Exec 'taskkill.exe /F /IM ${EXE}'
      Pop $R0
      Sleep 1000
      Return
    ${EndIf}
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL "$(MSG_CLOSE_APP)" IDRETRY +2
    Quit
  ${Loop}
FunctionEnd
!macroend
!insertmacro WAIT_CLIENT_CLOSED ""
!insertmacro WAIT_CLIENT_CLOSED "un."

Function .onInit
  SetShellVarContext current
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "$(MSG_NEED64)" /SD IDOK
    Abort
  ${EndIf}
FunctionEnd

; ------------------------------------------------------------------ sections

Section "$(NAME_SecApp)" SecApp
  SectionIn RO
  SetOutPath "$INSTDIR"
  Call WaitClientClosed
  ; Update: drop the DLLs of the previous version (their names carry FFmpeg's major version).
  Delete "$INSTDIR\*.dll"
  File "${STAGE}\${EXE}"
  File "${STAGE}\*.dll"
  File "${STAGE}\LICENSE.txt"
  SetOutPath "$INSTDIR\LICENSES"
  File /r "${STAGE}\LICENSES\*.*"
  SetOutPath "$INSTDIR"

  ; Start menu: one shortcut at the top level, like most apps.
  CreateShortcut "$SMPROGRAMS\2moon.lnk" "$INSTDIR\${EXE}"

  ; Settings > Apps (current user)
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "2moon"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${EXE}"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTKEY}" "URLInfoAbout" "https://github.com/RM4Y/2moon"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" $0
SectionEnd

Section "$(NAME_SecDesktop)" SecDesktop
  CreateShortcut "$DESKTOP\2moon.lnk" "$INSTDIR\${EXE}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} $(DESC_SecApp)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DESC_SecDesktop)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ------------------------------------------------------------------ uninstall

Function un.onInit
  SetShellVarContext current
FunctionEnd

Section "Uninstall"
  Call un.WaitClientClosed
  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\LICENSE.txt"
  RMDir /r "$INSTDIR\LICENSES"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\2moon.lnk"
  Delete "$DESKTOP\2moon.lnk"
  DeleteRegKey HKCU "${UNINSTKEY}"
  ; Silent uninstall keeps the settings (an uninstall + reinstall keeps pairings).
  MessageBox MB_YESNO|MB_DEFBUTTON2 "$(UN_ASK_DATA)" /SD IDNO IDNO +2
  RMDir /r "$APPDATA\2moon-client"
SectionEnd
