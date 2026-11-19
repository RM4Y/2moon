; 2moon host for Windows: installer (NSIS, built by scripts/build-windows-installer.sh).
;
; Installs moon-server.exe and its DLLs in Program Files, registers the "2moon"
; Windows service (LocalSystem, starts the host in the signed-in session, like
; Sunshine), allows it through the firewall, optionally installs the ViGEmBus
; driver for gamepads, and adds Start menu shortcuts to the admin panel and logs.
; Data (pairings, settings, logs) lives in %ProgramData%\2moon, readable by
; SYSTEM and Administrators only.
;
; Silent install / uninstall (deployment, winget):
;   2moon-host-<version>-windows-x86_64-setup.exe /S [/D=C:\path]
;   "C:\Program Files\2moon Host\uninstall.exe" /S     (keeps %ProgramData%\2moon)

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
!define FWRULE "2moon Host"

Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\2moon Host"
InstallDirRegKey HKLM "${UNINSTKEY}" "InstallLocation"
BrandingText "2moon ${VERSION}"

!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"
!include "Sections.nsh"
!include "FileFunc.nsh"

!define MUI_ICON "2moon.ico"
!define MUI_UNICON "2moon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "$(FINISH_RUN)"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenAdmin
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!define MUI_FINISHPAGE_SHOWREADME_TEXT "$(FINISH_LOGS)"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION OpenLogs

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
; The installer follows the Windows display language; the first one listed is
; the fallback for every other language, so English comes first.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "French"

; Version tab of the setup .exe properties (after MUI_LANGUAGE: needs ${LANG_ENGLISH}).
VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductName" "${APPNAME}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileDescription" "2moon Host installer"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "LegalCopyright" "GPL-3.0-or-later"

; ------------------------------------------------------------------ strings

LangString MSG_NEED64 ${LANG_ENGLISH} "2moon Host requires 64-bit Windows."
LangString MSG_NEED64 ${LANG_FRENCH} "2moon Host nécessite Windows 64 bits."
LangString MSG_SUNSHINE ${LANG_ENGLISH} "Sunshine is installed on this computer. It uses the same ports as 2moon: stop or uninstall the Sunshine service, otherwise 2moon cannot start.$\n$\nContinue the installation?"
LangString MSG_SUNSHINE ${LANG_FRENCH} "Sunshine est installé sur cet ordinateur. Il utilise les mêmes ports que 2moon : arrêtez ou désinstallez le service Sunshine, sinon 2moon ne pourra pas démarrer.$\n$\nContinuer l'installation ?"
LangString MSG_SERVICE_FAILED ${LANG_ENGLISH} "The 2moon service could not be installed (code $0). See the installation details."
LangString MSG_SERVICE_FAILED ${LANG_FRENCH} "Le service 2moon n'a pas pu être installé (code $0). Voir les détails de l'installation."
LangString MSG_STOPPING ${LANG_ENGLISH} "Stopping the 2moon host..."
LangString MSG_STOPPING ${LANG_FRENCH} "Arrêt de l'hôte 2moon..."
LangString MSG_KILLING ${LANG_ENGLISH} "The 2moon host is still running: forcing it to stop."
LangString MSG_KILLING ${LANG_FRENCH} "L'hôte 2moon tourne encore : arrêt forcé."
LangString MSG_STILL_RUNNING ${LANG_ENGLISH} "moon-server.exe is still in use; some files may not be replaced."
LangString MSG_STILL_RUNNING ${LANG_FRENCH} "moon-server.exe est encore utilisé ; certains fichiers risquent de ne pas être remplacés."
LangString MSG_VIGEM_PRESENT ${LANG_ENGLISH} "ViGEmBus is already installed."
LangString MSG_VIGEM_PRESENT ${LANG_FRENCH} "ViGEmBus est déjà installé."
LangString MSG_VIGEM_INSTALL ${LANG_ENGLISH} "Installing the ViGEmBus driver (virtual gamepads)..."
LangString MSG_VIGEM_INSTALL ${LANG_FRENCH} "Installation du pilote ViGEmBus (manettes virtuelles)..."
LangString FINISH_RUN ${LANG_ENGLISH} "Open the admin panel (create the account)"
LangString FINISH_RUN ${LANG_FRENCH} "Ouvrir le panneau d'administration (créer le compte)"
LangString FINISH_LOGS ${LANG_ENGLISH} "Open the log folder"
LangString FINISH_LOGS ${LANG_FRENCH} "Ouvrir le dossier des journaux"
LangString LNK_ADMIN ${LANG_ENGLISH} "2moon Administration"
LangString LNK_ADMIN ${LANG_FRENCH} "Administration 2moon"
LangString LNK_LOGS ${LANG_ENGLISH} "2moon Logs"
LangString LNK_LOGS ${LANG_FRENCH} "Journaux 2moon"
LangString LNK_UNINSTALL ${LANG_ENGLISH} "Uninstall 2moon Host"
LangString LNK_UNINSTALL ${LANG_FRENCH} "Désinstaller 2moon Host"
LangString NAME_SecHost ${LANG_ENGLISH} "2moon Host (service)"
LangString NAME_SecHost ${LANG_FRENCH} "2moon Host (service)"
LangString NAME_SecFwPublic ${LANG_ENGLISH} "Allow on public networks"
LangString NAME_SecFwPublic ${LANG_FRENCH} "Autoriser sur les réseaux publics"
LangString NAME_SecVigem ${LANG_ENGLISH} "Gamepads (ViGEmBus driver)"
LangString NAME_SecVigem ${LANG_FRENCH} "Manettes (pilote ViGEmBus)"
LangString DESC_SecHost ${LANG_ENGLISH} "The streaming host, run as a Windows service at every start. Reachable from private networks (home, work)."
LangString DESC_SecHost ${LANG_FRENCH} "L'hôte de streaming, lancé en service Windows à chaque démarrage. Accessible depuis les réseaux privés (domicile, travail)."
LangString DESC_SecFwPublic ${LANG_ENGLISH} "Also accept connections when Windows considers the network public (café, hotel Wi-Fi...). Only needed if your home network is set to Public."
LangString DESC_SecFwPublic ${LANG_FRENCH} "Accepter aussi les connexions quand Windows considère le réseau comme public (Wi-Fi de café, d'hôtel...). Utile seulement si votre réseau domestique est réglé sur Public."
LangString DESC_SecVigem ${LANG_ENGLISH} "Virtual Xbox 360 controller driver (ViGEmBus 1.22, Nefarius). Needed for gamepads."
LangString DESC_SecVigem ${LANG_FRENCH} "Pilote de manettes Xbox 360 virtuelles (ViGEmBus 1.22, Nefarius). Nécessaire pour jouer à la manette."
LangString UN_ASK_DATA ${LANG_ENGLISH} "Also delete the 2moon configuration (paired devices, admin account, logs)?"
LangString UN_ASK_DATA ${LANG_FRENCH} "Supprimer aussi la configuration de 2moon (appareils appairés, compte d'administration, journaux) ?"

; ------------------------------------------------------------------ helpers

; Wait until moon-server.exe is no longer running (its file can be opened for
; writing again), after the service was asked to stop. The service stops its
; host process itself; a host left behind (crashed service, manual run) is
; killed after 20 s so an update never fails on "Error opening file for writing".
!macro WAIT_HOST_STOPPED un
Function ${un}WaitHostStopped
  ${IfNot} ${FileExists} "$INSTDIR\moon-server.exe"
    Return
  ${EndIf}
  DetailPrint "$(MSG_STOPPING)"
  StrCpy $R1 0
  ${Do}
    ClearErrors
    FileOpen $R0 "$INSTDIR\moon-server.exe" a
    ${IfNot} ${Errors}
      FileClose $R0
      Return
    ${EndIf}
    IntOp $R1 $R1 + 1
    ${If} $R1 = 40
      DetailPrint "$(MSG_KILLING)"
      nsExec::Exec 'taskkill.exe /F /IM moon-server.exe'
      Pop $R0
    ${ElseIf} $R1 > 60
      DetailPrint "$(MSG_STILL_RUNNING)"
      Return
    ${EndIf}
    Sleep 500
  ${Loop}
FunctionEnd
!macroend
!insertmacro WAIT_HOST_STOPPED ""
!insertmacro WAIT_HOST_STOPPED "un."

Function .onInit
  ; All users: $APPDATA is C:\ProgramData, shortcuts go in the common Start menu.
  SetShellVarContext all
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "$(MSG_NEED64)" /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
  ; Sunshine listens on the same ports (47984-48010): both cannot run at once.
  ; Silent install: carry on (the files install fine; the service waits for the ports).
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Services\SunshineService" "ImagePath"
  ${If} $0 != ""
    MessageBox MB_ICONEXCLAMATION|MB_OKCANCEL "$(MSG_SUNSHINE)" /SD IDOK IDOK +2
    Abort
  ${EndIf}
  Call InitFirewallChoice
FunctionEnd

Function OpenAdmin
  ExecShell "open" "${ADMIN_URL}"
FunctionEnd

Function OpenLogs
  ExecShell "open" "$APPDATA\2moon\logs"
FunctionEnd

; ------------------------------------------------------------------ sections

Section "$(NAME_SecHost)" SecHost
  SectionIn RO
  SetRegView 64
  SetOutPath "$INSTDIR"
  ; Upgrade: stop the running service (and its host) so its files can be replaced.
  nsExec::Exec 'sc.exe stop 2moon'
  Pop $0
  Call WaitHostStopped
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
  Pop $0

  ; Firewall: let clients reach the host (GameStream ports and the admin panel, all
  ; on this program). Private and domain networks only; the "public networks"
  ; component below widens it.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${FWRULE}"'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall add rule name="${FWRULE}" dir=in action=allow program="$INSTDIR\moon-server.exe" enable=yes profile=private,domain'
  Pop $0
  WriteRegDWORD HKLM "${UNINSTKEY}" "FirewallPublic" 0

  ; The service (replaces an earlier registration) and its first start.
  nsExec::ExecToLog '"$INSTDIR\moon-server.exe" --install-service'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "$(MSG_SERVICE_FAILED)" /SD IDOK
  ${EndIf}

  ; Start menu (recreated: drops shortcuts named in another language by an earlier install)
  RMDir /r "$SMPROGRAMS\2moon Host"
  CreateDirectory "$SMPROGRAMS\2moon Host"
  ; A web link shows the icon named inside it (a .lnk pointing at it would not).
  Delete "$INSTDIR\Administration 2moon.url"
  WriteINIStr "$SMPROGRAMS\2moon Host\$(LNK_ADMIN).url" "InternetShortcut" "URL" "${ADMIN_URL}"
  WriteINIStr "$SMPROGRAMS\2moon Host\$(LNK_ADMIN).url" "InternetShortcut" "IconFile" "$INSTDIR\moon-server.exe"
  WriteINIStr "$SMPROGRAMS\2moon Host\$(LNK_ADMIN).url" "InternetShortcut" "IconIndex" "0"
  CreateShortcut "$SMPROGRAMS\2moon Host\$(LNK_LOGS).lnk" "$APPDATA\2moon\logs" "" "$INSTDIR\moon-server.exe" 0
  CreateShortcut "$SMPROGRAMS\2moon Host\$(LNK_UNINSTALL).lnk" "$INSTDIR\uninstall.exe"

  ; Programs and Features
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "Publisher" "2moon"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\moon-server.exe,0"
  WriteRegStr HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKLM "${UNINSTKEY}" "URLInfoAbout" "https://github.com/RM4Y/2moon"
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${UNINSTKEY}" "EstimatedSize" $0
SectionEnd

Section /o "$(NAME_SecFwPublic)" SecFwPublic
  SetRegView 64
  nsExec::Exec 'netsh advfirewall firewall set rule name="${FWRULE}" new profile=any'
  Pop $0
  WriteRegDWORD HKLM "${UNINSTKEY}" "FirewallPublic" 1
SectionEnd

Section "$(NAME_SecVigem)" SecVigem
  SetRegView 64
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Services\ViGEmBus" "ImagePath"
  ${If} $0 != ""
    DetailPrint "$(MSG_VIGEM_PRESENT)"
  ${Else}
    SetOutPath "$PLUGINSDIR"
    File "${STAGE}\ViGEmBus_setup.exe"
    DetailPrint "$(MSG_VIGEM_INSTALL)"
    ${If} ${Silent}
      ExecWait '"$PLUGINSDIR\ViGEmBus_setup.exe" /quiet /norestart' $1
    ${Else}
      ExecWait '"$PLUGINSDIR\ViGEmBus_setup.exe" /passive /norestart' $1
    ${EndIf}
    DetailPrint "ViGEmBus : code $1"
    ; The host connects to the driver when a stream starts: restart it to be sure.
    nsExec::Exec 'sc.exe stop 2moon'
    Pop $0
    Call WaitHostStopped
    nsExec::Exec 'sc.exe start 2moon'
    Pop $0
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecHost} $(DESC_SecHost)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecFwPublic} $(DESC_SecFwPublic)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVigem} $(DESC_SecVigem)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; Preselect "public networks" when the previous install had it, and on updates
; from versions before that option (they opened every network profile), so an
; update never cuts off a working setup. (After the sections: needs ${SecFwPublic}.)
Function InitFirewallChoice
  ReadRegStr $0 HKLM "${UNINSTKEY}" "InstallLocation"
  ClearErrors
  ReadRegDWORD $1 HKLM "${UNINSTKEY}" "FirewallPublic"
  ${If} ${Errors}
    ${If} $0 != ""
      !insertmacro SelectSection ${SecFwPublic}
    ${EndIf}
  ${ElseIf} $1 = 1
    !insertmacro SelectSection ${SecFwPublic}
  ${EndIf}
FunctionEnd

; ------------------------------------------------------------------ uninstall

Function un.onInit
  SetShellVarContext all
FunctionEnd

Section "Uninstall"
  SetRegView 64
  nsExec::ExecToLog '"$INSTDIR\moon-server.exe" --uninstall-service'
  Pop $0
  Call un.WaitHostStopped
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${FWRULE}"'
  Pop $0
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
  ; Silent uninstall keeps the configuration (an uninstall + reinstall keeps pairings).
  MessageBox MB_YESNO|MB_DEFBUTTON2 "$(UN_ASK_DATA)" /SD IDNO IDNO +2
  RMDir /r "$APPDATA\2moon"
  ; ViGEmBus is left installed: other programs may use it (Programs and Features to remove it).
SectionEnd
