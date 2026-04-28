; diskvis NSIS installer script
; Builds: makensis installer/diskvis.nsi
;
; Inputs (relative to this script):
;   ..\diskvis-windows-x86_64.exe   - the release binary
;
; Output:
;   ..\diskvis-setup-x86_64.exe     - the installer

!define APP_NAME       "diskvis"
!define APP_PUBLISHER  "TheOriUHD"
!define APP_VERSION    "1.3.0"
!define APP_EXE        "diskvis.exe"
!define APP_INSTALLDIR "$PROGRAMFILES64\diskvis"
!define APP_REGKEY     "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis"
!define APP_URL        "https://github.com/TheOriUHD/diskvis"

Name "${APP_NAME}"
OutFile "..\diskvis-setup-x86_64.exe"
InstallDir "${APP_INSTALLDIR}"
InstallDirRegKey HKLM "Software\${APP_NAME}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
Unicode true

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "WinMessages.nsh"

!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

!define ENV_REGKEY 'HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"'

; Add $INSTDIR to the machine PATH (idempotent: appends only if not present).
Function AddToPath
    ReadRegStr $0 ${ENV_REGKEY} "Path"
    StrCpy $1 ";$0;"
    StrCpy $2 ";$INSTDIR;"
    Push $1
    Push $2
    Call StrContains
    Pop $3
    ${If} $3 == ""
        ${If} $0 == ""
            StrCpy $0 "$INSTDIR"
        ${Else}
            StrCpy $0 "$0;$INSTDIR"
        ${EndIf}
        WriteRegExpandStr ${ENV_REGKEY} "Path" "$0"
        SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=5000
    ${EndIf}
FunctionEnd

Function un.RemoveFromPath
    ReadRegStr $0 ${ENV_REGKEY} "Path"
    Push $0
    Push "$INSTDIR"
    Call un.StrRemove
    Pop $0
    WriteRegExpandStr ${ENV_REGKEY} "Path" "$0"
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=5000
FunctionEnd

; StrContains: returns matched substring on stack, or empty string if not found.
;   Push haystack
;   Push needle
;   Call StrContains
;   Pop $result
Function StrContains
    Exch $R1 ; needle
    Exch
    Exch $R2 ; haystack
    Push $R3
    Push $R4
    Push $R5
    StrLen $R3 $R1
    StrCpy $R4 0
    loop:
        StrCpy $R5 $R2 $R3 $R4
        StrCmp $R5 $R1 found
        StrCmp $R5 "" notfound
        IntOp $R4 $R4 + 1
        Goto loop
    found:
        StrCpy $R1 $R2 "" $R4
        Goto done
    notfound:
        StrCpy $R1 ""
    done:
        Pop $R5
        Pop $R4
        Pop $R3
        Pop $R2
        Exch $R1
FunctionEnd

; un.StrRemove: remove all occurrences of `;needle` and `needle;` and bare `needle` from haystack.
;   Push haystack
;   Push needle
;   Call un.StrRemove
;   Pop $result
Function un.StrRemove
    Exch $R1 ; needle
    Exch
    Exch $R0 ; haystack
    Push $R2
    Push $R3
    Push $R4
    Push $R5
    StrCpy $R2 ""
    StrLen $R3 $R1
    StrCpy $R4 0
    rloop:
        StrCpy $R5 $R0 1 $R4
        StrCmp $R5 "" rdone
        StrCpy $R5 $R0 $R3 $R4
        StrCmp $R5 $R1 rmatch
        StrCpy $R5 $R0 1 $R4
        StrCpy $R2 "$R2$R5"
        IntOp $R4 $R4 + 1
        Goto rloop
    rmatch:
        IntOp $R4 $R4 + $R3
        Goto rloop
    rdone:
        ; collapse any "::" left behind
        StrCpy $R0 $R2
        StrCpy $R2 ""
        StrCpy $R4 0
    rcloop:
        StrCpy $R5 $R0 1 $R4
        StrCmp $R5 "" rcdone
        StrCpy $R3 $R0 2 $R4
        ${If} $R3 == ";;"
            StrCpy $R2 "$R2;"
            IntOp $R4 $R4 + 2
            Goto rcloop
        ${EndIf}
        StrCpy $R2 "$R2$R5"
        IntOp $R4 $R4 + 1
        Goto rcloop
    rcdone:
        StrCpy $R0 $R2
        Pop $R5
        Pop $R4
        Pop $R3
        Pop $R2
        Pop $R1
        Exch $R0
FunctionEnd

Section "diskvis (required)" SecCore
    SectionIn RO
    SetOutPath "$INSTDIR"

    File /oname=${APP_EXE} "..\diskvis-windows-x86_64.exe"
    File "..\README.md"
    File "..\LICENSE"

    Call AddToPath

    WriteRegStr HKLM "Software\${APP_NAME}" "InstallDir" "$INSTDIR"
    WriteRegStr HKLM "${APP_REGKEY}" "DisplayName" "${APP_NAME}"
    WriteRegStr HKLM "${APP_REGKEY}" "DisplayVersion" "${APP_VERSION}"
    WriteRegStr HKLM "${APP_REGKEY}" "Publisher" "${APP_PUBLISHER}"
    WriteRegStr HKLM "${APP_REGKEY}" "URLInfoAbout" "${APP_URL}"
    WriteRegStr HKLM "${APP_REGKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegStr HKLM "${APP_REGKEY}" "InstallLocation" "$INSTDIR"
    WriteRegDWORD HKLM "${APP_REGKEY}" "NoModify" 1
    WriteRegDWORD HKLM "${APP_REGKEY}" "NoRepair" 1

    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "DisplayName" "diskvis"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "UninstallString" "$INSTDIR\uninstall.exe"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "DisplayVersion" "1.3.0"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "Publisher" "TheOriUHD"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "DisplayIcon" "$INSTDIR\diskvis.exe"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis" "NoRepair" 1
SectionEnd

Section "Start Menu shortcut" SecShortcut
    CreateDirectory "$SMPROGRAMS\${APP_NAME}"

    ; Launch via Windows Terminal so the TUI gets a proper pseudoconsole.
    ; %USERPROFILE% is expanded at launch time so the working dir is always
    ; the current user's home.
    CreateShortcut "$SMPROGRAMS\${APP_NAME}\diskvis.lnk" \
        "wt.exe" \
        '-d "%USERPROFILE%" "$INSTDIR\${APP_EXE}"' \
        "$INSTDIR\${APP_EXE}" 0 \
        SW_SHOWNORMAL "" "Run diskvis in your home directory"

    CreateShortcut "$SMPROGRAMS\${APP_NAME}\Uninstall diskvis.lnk" \
        "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
    Call un.RemoveFromPath

    Delete "$INSTDIR\${APP_EXE}"
    Delete "$INSTDIR\README.md"
    Delete "$INSTDIR\LICENSE"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir  "$INSTDIR"

    Delete "$SMPROGRAMS\${APP_NAME}\diskvis.lnk"
    Delete "$SMPROGRAMS\${APP_NAME}\Uninstall diskvis.lnk"
    RMDir  "$SMPROGRAMS\${APP_NAME}"

    DeleteRegKey HKLM "${APP_REGKEY}"
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\diskvis"
    DeleteRegKey HKLM "Software\${APP_NAME}"
SectionEnd
