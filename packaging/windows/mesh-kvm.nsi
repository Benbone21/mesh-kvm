; The Windows installer, mesh-kvm-setup.exe. packaging/windows/build-setup.sh builds it.
;
; It installs for this user only, so it never asks for admin. The exe goes where it always has,
; %LOCALAPPDATA%\mesh-kvm\mesh-kvm.exe, beside the settings: the updater, the firewall rule and
; earlier installs all expect it there. The exe itself writes the Start menu entry and the login
; script, through `--install-launchers`, so that logic stays in one place.
;
; Silent use: mesh-kvm-setup.exe /S [/AUTOSTART=on|off]. Without /AUTOSTART, a reinstall keeps the
; choice made before, and a first install turns it on. The uninstaller takes /S as well, and then
; keeps the settings.

Unicode true
!ifndef VERSION
  !error "Give the version: -DVERSION=0.7.2"
!endif
!ifndef EXE
  !error "Give the Windows exe: -DEXE=path/to/mesh-kvm.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "mesh-kvm-setup.exe"
!endif

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\mesh-kvm"

Name "mesh-kvm"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\mesh-kvm"
SetCompressor /SOLID lzma
ShowInstDetails nevershow
ShowUninstDetails nevershow
BrandingText "mesh-kvm ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "mesh-kvm"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "mesh-kvm setup"
VIAddVersionKey "CompanyName" "mesh-kvm"
VIAddVersionKey "LegalCopyright" "mesh-kvm"

!include MUI2.nsh
!include LogicLib.nsh
!include FileFunc.nsh
!include nsDialogs.nsh

!ifdef ICON
  !define MUI_ICON "${ICON}"
  !define MUI_UNICON "${ICON}"
!endif

Var Autostart
Var AutostartBox

!define MUI_WELCOMEPAGE_TEXT "This installs mesh-kvm ${VERSION} for you on this computer. No administrator rights are needed.$\r$\n$\r$\nIf mesh-kvm is running, it stops while it's installed. Your settings are kept."
!insertmacro MUI_PAGE_WELCOME
Page custom StartupPage StartupPageLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open mesh-kvm"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenMeshKvm
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Asks every mesh-kvm here to stop: the service first, through its own quit, which gives back any
; output "Play only on the other device" muted. Then Windows asks what's left to close, and after
; that it's ended. $0 is the exe that asks.
!macro StopMeshKvm
  ${If} ${FileExists} "$0"
    ExecWait '"$0" --quit'
  ${EndIf}
  nsExec::Exec 'taskkill /IM mesh-kvm.exe'
  Pop $1
  nsExec::Exec 'taskkill /IM mesh-kvm-recover.exe'
  Pop $1
  Sleep 2000
  nsExec::Exec 'taskkill /F /IM mesh-kvm.exe'
  Pop $1
  nsExec::Exec 'taskkill /F /IM mesh-kvm-recover.exe'
  Pop $1
  Sleep 500
!macroend

Function .onInit
  SetShellVarContext current
  ; A reinstall keeps the choice made before.
  StrCpy $Autostart 1
  ${If} ${FileExists} "$INSTDIR\autostart-disabled"
    StrCpy $Autostart 0
  ${EndIf}
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/AUTOSTART=" $R1
  ${IfNot} ${Errors}
    ${If} $R1 == "off"
      StrCpy $Autostart 0
    ${ElseIf} $R1 == "on"
      StrCpy $Autostart 1
    ${EndIf}
  ${EndIf}
FunctionEnd

Function StartupPage
  !insertmacro MUI_HEADER_TEXT "Starting mesh-kvm" "Choose whether mesh-kvm starts by itself."
  nsDialogs::Create 1018
  Pop $R0
  ${If} $R0 == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 0 0 100% 36u "With this on, sharing starts when you sign in to Windows, with the tray icon. You can change it later under Startup in the app."
  Pop $R0
  ${NSD_CreateCheckbox} 0 44u 100% 12u "Start mesh-kvm when I sign in"
  Pop $AutostartBox
  ${If} $Autostart == 1
    ${NSD_Check} $AutostartBox
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function StartupPageLeave
  ${NSD_GetState} $AutostartBox $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $Autostart 1
  ${Else}
    StrCpy $Autostart 0
  ${EndIf}
FunctionEnd

Function OpenMeshKvm
  ; With no arguments, the installed exe opens the window and the tray, and starts sharing.
  Exec '"$INSTDIR\mesh-kvm.exe"'
FunctionEnd

Section "mesh-kvm"
  SetShellVarContext current
  InitPluginsDir
  SetOutPath "$INSTDIR"
  File "/oname=$PLUGINSDIR\mesh-kvm.exe" "${EXE}"
  StrCpy $0 "$PLUGINSDIR\mesh-kvm.exe"
  !insertmacro StopMeshKvm

  ; What an update left behind doesn't apply to a fresh install.
  Delete "$INSTDIR\mesh-kvm.exe.old"
  Delete "$INSTDIR\mesh-kvm.exe.new"
  Delete "$INSTDIR\mesh-kvm.exe.failed"
  Delete "$INSTDIR\mesh-kvm-recover.exe"
  Delete "$INSTDIR\update-pending.json"
  Delete "$INSTDIR\update-note.txt"

  ; Windows can rename an exe that's still running, though it can't replace it.
  Delete "$INSTDIR\mesh-kvm.exe"
  ${If} ${FileExists} "$INSTDIR\mesh-kvm.exe"
    Rename "$INSTDIR\mesh-kvm.exe" "$INSTDIR\mesh-kvm.exe.old"
  ${EndIf}
  ClearErrors
  CopyFiles /SILENT "$PLUGINSDIR\mesh-kvm.exe" "$INSTDIR\mesh-kvm.exe"
  ${If} ${Errors}
    MessageBox MB_OK|MB_ICONSTOP "mesh-kvm couldn't be copied to $INSTDIR. Close anything using it and run the setup again." /SD IDOK
    Abort
  ${EndIf}

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "mesh-kvm"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "mesh-kvm"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\mesh-kvm.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/M=mesh-kvm.exe /S=0K" $R0 $R1 $R2
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" $R0

  ${If} $Autostart == 1
    ExecWait '"$INSTDIR\mesh-kvm.exe" --install-launchers --autostart on'
  ${Else}
    ExecWait '"$INSTDIR\mesh-kvm.exe" --install-launchers --autostart off'
  ${EndIf}

  ; A silent install has no finish page, so it starts what a sign-in would.
  ${If} ${Silent}
  ${AndIf} $Autostart == 1
    Exec '"$INSTDIR\mesh-kvm.exe" --tray'
  ${EndIf}
SectionEnd

Function un.onInit
  SetShellVarContext current
FunctionEnd

Section "Uninstall"
  SetShellVarContext current
  StrCpy $0 "$INSTDIR\mesh-kvm.exe"
  !insertmacro StopMeshKvm

  Delete "$INSTDIR\mesh-kvm.exe"
  Delete "$INSTDIR\mesh-kvm.exe.old"
  Delete "$INSTDIR\mesh-kvm.exe.new"
  Delete "$INSTDIR\mesh-kvm.exe.failed"
  Delete "$INSTDIR\mesh-kvm-recover.exe"
  Delete "$INSTDIR\update-pending.json"
  Delete "$INSTDIR\update-note.txt"
  RMDir /r "$INSTDIR\updates"
  Delete "$SMPROGRAMS\mesh-kvm.lnk"
  Delete "$SMPROGRAMS\mesh-kvm.vbs"
  Delete "$SMPROGRAMS\mesh-kvm Studio.vbs"
  Delete "$SMSTARTUP\mesh-kvm-autostart.vbs"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"

  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also delete your mesh-kvm settings?$\r$\n$\r$\nThat's the other device's address, the layout of your screens, and your choices in the app. Keep them if you might install mesh-kvm again." /SD IDNO IDYES forget
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Goto done
  forget:
  RMDir /r "$INSTDIR"
  RMDir /r "$APPDATA\mesh-kvm"
  done:
SectionEnd
