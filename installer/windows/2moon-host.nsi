; 2moon host for Windows: installer (NSIS, built by scripts/build-windows-installer.sh).
;
; Installs moon-server.exe and its DLLs in Program Files, registers the "2moon"
; Windows service (LocalSystem, starts the host in the signed-in session, like
; Sunshine), allows it through the firewall, optionally installs the ViGEmBus
; driver for gamepads, and adds Start menu shortcuts to the admin panel and logs.
; Data (pairings, settings, logs) lives in %ProgramData%\2moon, readable by
; SYSTEM and Administrators only.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel admin

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef STAGE
  !define STAGE "stage"
!endif
!ifndef OUTFILE
  !define OUTFILE "2moon-host-setup.exe"
!endif

!define APPNAME "2moon Host"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\2moon-host"
!define ADMIN_URL "https://localhost:47990/"

Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\2moon Host"
InstallDirRegKey HKLM "${UNINSTKEY}" "InstallLocation"
BrandingText "2moon ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "FileDescription" "2moon Host installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "GPL-3.0-or-later"

!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"

!define MUI_ICON "2moon.ico"
!define MUI_UNICON "2moon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Ouvrir le panneau d'administration (créer le compte)"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenAdmin
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Ouvrir le dossier des journaux"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION OpenLogs

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "French"
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ; All users: $APPDATA is C:\ProgramData, shortcuts go in the common Start menu.
  SetShellVarContext all
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "2moon Host nécessite Windows 64 bits."
    Abort
  ${EndIf}
  SetRegView 64
  ; Sunshine listens on the same ports (47984-48010): both cannot run at once.
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Services\SunshineService" "ImagePath"
  ${If} $0 != ""
    MessageBox MB_ICONEXCLAMATION|MB_OKCANCEL "Sunshine est installé sur cet ordinateur. Il utilise les mêmes ports que 2moon : arrêtez ou désinstallez le service Sunshine, sinon 2moon ne pourra pas démarrer.$\n$\nContinuer l'installation ?" IDOK +2
    Abort
  ${EndIf}
FunctionEnd

Function OpenAdmin
  ExecShell "open" "${ADMIN_URL}"
FunctionEnd

Function OpenLogs
  ExecShell "open" "$APPDATA\2moon\logs"
FunctionEnd

Section "2moon Host (service)" SecHost
  SectionIn RO
  SetRegView 64
  SetOutPath "$INSTDIR"
  ; Upgrade: stop the running service so its files can be replaced.
  nsExec::Exec 'sc.exe stop 2moon'
  Sleep 2000
  File "${STAGE}\moon-server.exe"
  File "${STAGE}\*.dll"
  File "${STAGE}\LICENSE.txt"
  File "${STAGE}\README.txt"
  File "2moon.ico"
  SetOutPath "$INSTDIR\LICENSES"
  File /r "${STAGE}\LICENSES\*.*"

  ; Data folder: SYSTEM and Administrators only (host key, pairings, admin account).
  CreateDirectory "$APPDATA\2moon\logs"
  nsExec::Exec 'icacls "$APPDATA\2moon" /inheritance:r /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F'

  ; Firewall: let clients reach the host (GameStream ports, all on this program).
  nsExec::Exec 'netsh advfirewall firewall delete rule name="2moon Host"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="2moon Host" dir=in action=allow program="$INSTDIR\moon-server.exe" enable=yes profile=any'

  ; The service (replaces an earlier registration) and its first start.
  nsExec::ExecToLog '"$INSTDIR\moon-server.exe" --install-service'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "Le service 2moon n'a pas pu être installé (code $0). Voir les détails de l'installation."
  ${EndIf}

  ; Start menu
  CreateDirectory "$SMPROGRAMS\2moon Host"
  ; A web link shows the icon named inside it (a .lnk pointing at it would not).
  Delete "$SMPROGRAMS\2moon Host\Administration 2moon.lnk"
  Delete "$INSTDIR\Administration 2moon.url"
  WriteINIStr "$SMPROGRAMS\2moon Host\Administration 2moon.url" "InternetShortcut" "URL" "${ADMIN_URL}"
  WriteINIStr "$SMPROGRAMS\2moon Host\Administration 2moon.url" "InternetShortcut" "IconFile" "$INSTDIR\moon-server.exe"
  WriteINIStr "$SMPROGRAMS\2moon Host\Administration 2moon.url" "InternetShortcut" "IconIndex" "0"
  CreateShortcut "$SMPROGRAMS\2moon Host\Journaux 2moon.lnk" "$APPDATA\2moon\logs" "" "$INSTDIR\moon-server.exe" 0
  CreateShortcut "$SMPROGRAMS\2moon Host\Désinstaller 2moon Host.lnk" "$INSTDIR\uninstall.exe"

  ; Programs and Features
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "Publisher" "2moon"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\moon-server.exe,0"
  WriteRegStr HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINSTKEY}" "URLInfoAbout" "https://github.com/RM4Y/2moon"
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1
SectionEnd

Section "Manettes (pilote ViGEmBus)" SecVigem
  SetRegView 64
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Services\ViGEmBus" "ImagePath"
  ${If} $0 != ""
    DetailPrint "ViGEmBus est déjà installé."
  ${Else}
    SetOutPath "$PLUGINSDIR"
    File "${STAGE}\ViGEmBus_setup.exe"
    DetailPrint "Installation du pilote ViGEmBus (manettes virtuelles)..."
    ExecWait '"$PLUGINSDIR\ViGEmBus_setup.exe" /passive /norestart' $1
    DetailPrint "ViGEmBus : code $1"
    ; The host connects to the driver when a stream starts: restart it to be sure.
    nsExec::Exec 'sc.exe stop 2moon'
    Sleep 2000
    nsExec::Exec 'sc.exe start 2moon'
  ${EndIf}
SectionEnd

LangString DESC_SecHost ${LANG_FRENCH} "L'hôte de streaming, lancé en service Windows à chaque démarrage."
LangString DESC_SecHost ${LANG_ENGLISH} "The streaming host, run as a Windows service at every start."
LangString DESC_SecVigem ${LANG_FRENCH} "Pilote de manettes Xbox 360 virtuelles (ViGEmBus 1.22, Nefarius). Nécessaire pour jouer à la manette."
LangString DESC_SecVigem ${LANG_ENGLISH} "Virtual Xbox 360 controller driver (ViGEmBus 1.22, Nefarius). Needed for gamepads."
!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecHost} $(DESC_SecHost)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVigem} $(DESC_SecVigem)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function un.onInit
  SetShellVarContext all
FunctionEnd

Section "Uninstall"
  SetRegView 64
  nsExec::ExecToLog '"$INSTDIR\moon-server.exe" --uninstall-service'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="2moon Host"'
  Delete "$INSTDIR\moon-server.exe"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\README.txt"
  Delete "$INSTDIR\2moon.ico"
  Delete "$INSTDIR\Administration 2moon.url"
  RMDir /r "$INSTDIR\LICENSES"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  RMDir /r "$SMPROGRAMS\2moon Host"
  DeleteRegKey HKLM "${UNINSTKEY}"
  MessageBox MB_YESNO|MB_DEFBUTTON2 "Supprimer aussi la configuration de 2moon (appareils appairés, compte d'administration, journaux) ?" IDNO +2
  RMDir /r "$APPDATA\2moon"
  ; ViGEmBus is left installed: other programs may use it (Programs and Features to remove it).
SectionEnd
