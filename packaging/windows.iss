#ifndef Version
  #error Version must be supplied with /DVersion
#endif

[Setup]
AppId={{BA33B2B4-2ADB-4AF0-87CD-F5439DCD6998}
AppName=DOWAI
AppVersion={#Version}
DefaultDirName={autopf64}\DOWAI
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesEnvironment=yes
OutputDir=..\dist
OutputBaseFilename=dowai-{#Version}-windows-x64-setup
Compression=lzma2
SolidCompression=yes

[Files]
Source: "..\target\release\dowai.exe"; DestDir: "{app}"; Flags: ignoreversion

[Code]
const
  LocalMachine = $80000002;
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  StateKey = 'SOFTWARE\DOWAI\Installer';
  StateValue = 'PathAdded';
  KeyRead = $20019;
  RegSz = 1;
  RegExpandSz = 2;

function RegOpenKeyEx(hKey: Integer; lpSubKey: String; ulOptions: Cardinal;
  samDesired: Cardinal; var phkResult: Integer): Longint;
  external 'RegOpenKeyExW@advapi32.dll stdcall';
function RegQueryValueEx(hKey: Integer; lpValueName: String; lpReserved: Integer;
  var lpType: Cardinal; lpData: Integer; lpcbData: Integer): Longint;
  external 'RegQueryValueExW@advapi32.dll stdcall';
function RegCloseKey(hKey: Integer): Longint;
  external 'RegCloseKey@advapi32.dll stdcall';
function ExpandEnvironmentStrings(lpSrc: String; lpDst: String; nSize: Cardinal): Cardinal;
  external 'ExpandEnvironmentStringsW@kernel32.dll stdcall';

function ReadPath(var Value: String; var ValueType: Cardinal): Boolean;
var
  Key: Integer;
begin
  Result := False;
  if RegOpenKeyEx(LocalMachine, EnvKey, 0, KeyRead, Key) <> 0 then
    Exit;
  try
    if RegQueryValueEx(Key, 'Path', 0, ValueType, 0, 0) <> 0 then
      Exit;
  finally
    RegCloseKey(Key);
  end;
  Result := ((ValueType = RegSz) or (ValueType = RegExpandSz)) and
    RegQueryStringValue(HKLM, EnvKey, 'Path', Value);
end;

function WritePath(const Value: String; ValueType: Cardinal): Boolean;
begin
  if ValueType = RegExpandSz then
    Result := RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Value)
  else
    Result := RegWriteStringValue(HKLM, EnvKey, 'Path', Value);
end;

function NormalizePath(Value: String): String;
var
  Expanded: String;
  Size: Cardinal;
begin
  Value := Trim(Value);
  if (Length(Value) >= 2) and (Value[1] = '"') and
    (Value[Length(Value)] = '"') then
    Value := Copy(Value, 2, Length(Value) - 2);
  StringChangeEx(Value, '%ProgramFiles%', ExpandConstant('{commonpf64}'), True);
  SetLength(Expanded, 32768);
  Size := ExpandEnvironmentStrings(Value, Expanded, 32768);
  if (Size = 0) or (Size > 32768) then
    RaiseException('Could not expand a system PATH entry.');
  SetLength(Expanded, Size - 1);
  StringChangeEx(Expanded, '/', '\', True);
  Result := Lowercase(RemoveBackslashUnlessRoot(Trim(Expanded)));
end;

function FindPathEntry(const Value, Dir: String; var Start, Count: Integer): Boolean;
var
  I, EntryStart: Integer;
  AtEnd: Boolean;
  Entry: String;
begin
  Result := False;
  EntryStart := 1;
  for I := 1 to Length(Value) + 1 do begin
    AtEnd := I > Length(Value);
    if not AtEnd then
      AtEnd := Value[I] = ';';
    if AtEnd then begin
      Entry := Copy(Value, EntryStart, I - EntryStart);
      if (Entry <> '') and (NormalizePath(Entry) = NormalizePath(Dir)) then begin
        Start := EntryStart;
        Count := I - EntryStart;
        Result := True;
      end;
      EntryStart := I + 1;
    end;
  end;
end;

procedure AddToPath;
var
  Value, NewValue: String;
  ValueType, Ownership: Cardinal;
  Start, Count: Integer;
begin
  if not ReadPath(Value, ValueType) then
    RaiseException('Could not read the system PATH.');
  if FindPathEntry(Value, ExpandConstant('{app}'), Start, Count) then
    Exit;
  NewValue := Value;
  Ownership := 1;
  if (NewValue <> '') and (NewValue[Length(NewValue)] <> ';') then begin
    NewValue := NewValue + ';';
    Ownership := 2;
  end;
  NewValue := NewValue + ExpandConstant('{app}');
  if Length(NewValue) >= 32767 then
    RaiseException('The system PATH is too long.');
  if not RegWriteDWordValue(HKLM, StateKey, StateValue, Ownership) then
    RaiseException('Could not record ownership of the system PATH entry.');
  if not WritePath(NewValue, ValueType) then begin
    RegDeleteValue(HKLM, StateKey, StateValue);
    RaiseException('Could not update the system PATH.');
  end;
end;

procedure RemoveFromPath;
var
  Value: String;
  ValueType, Added: Cardinal;
  Start, Count: Integer;
begin
  if not RegQueryDWordValue(HKLM, StateKey, StateValue, Added) then
    Exit;
  if (Added <> 1) and (Added <> 2) then
    RaiseException('Invalid PATH ownership record.');
  if not ReadPath(Value, ValueType) then
    RaiseException('Could not read the system PATH.');
  if FindPathEntry(Value, ExpandConstant('{app}'), Start, Count) then begin
    if (Added = 2) and (Start > 1) then begin
      Start := Start - 1;
      Count := Count + 1;
    end else if Length(Value) > Count then
      Count := Count + 1;
    Delete(Value, Start, Count);
    if not WritePath(Value, ValueType) then
      RaiseException('Could not update the system PATH.');
  end;
  if not RegDeleteValue(HKLM, StateKey, StateValue) then
    RaiseException('Could not remove the PATH ownership record.');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    AddToPath;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveFromPath;
end;
