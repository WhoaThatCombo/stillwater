<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="Stillwater"
           Manufacturer="Stillwater contributors"
           UpgradeCode="603d88ba-b9d4-4932-8667-ecdfc2b94e0e"
           Language="1033"
           Codepage="1252"
           Version="0.1.1"
           InstallerVersion="200">
    <SummaryInformation Keywords="Installer"
                        Description="Stillwater Installer"
                        Manufacturer="Stillwater contributors"/>
    <MajorUpgrade AllowDowngrades="yes"/>
    <Media Id="1"
           Cabinet="Stillwater.cab"
           EmbedCab="yes"/>
    <StandardDirectory Id="ProgramFiles64Folder">
      <Directory Id="StillwaterRoot" Name="Stillwater">
        <Directory Id="INSTALLDIR" Name="Stillwater">
            <Component Id="StillwaterMain"
                       Guid="944d7db1-de0f-4eaf-a11c-15aa80abd451"
                       Bitness="always64">
              <File Id="ServoEXE"
                    Name="Stillwater.exe"
                    DiskId="1"
                    Source="${windowize(exe_path)}\servoshell.exe"
                    KeyPath="yes">
              </File>
	            ${include_dependencies()}
            </Component>

            ${include_directory(resources_path, "resources")}
          </Directory>
        </Directory>
      </StandardDirectory>

      <StandardDirectory Id="ProgramMenuFolder">
        <Directory Id="ProgramMenuDir" Name="Stillwater">
          <Component Id="ProgramMenuDir" Guid="3b5c77db-ca91-4349-884a-efa4f0be77bf">
            <RemoveFolder Id="ProgramMenuDir" On="both"/>
            <RegistryValue Root="HKCU"
                           Key="Software\Stillwater"
                           Type="string"
                           Value=""
                           KeyPath="yes"/>
            <Shortcut Id="StartMenuStillwater"
              Directory="ProgramMenuDir"
              Name="Stillwater"
              Target="[INSTALLDIR]Stillwater.exe"
              WorkingDirectory="INSTALLDIR"
              Icon="servoshell.exe"/>
          </Component>
        </Directory>
      </StandardDirectory>

      <Feature Id="Complete" Level="1">
        <ComponentRef Id="StillwaterMain"/>
         % for c in components:
         <ComponentRef Id="${c}"/>
         % endfor
        <ComponentRef Id="ProgramMenuDir"/>
      </Feature>

      <Icon Id="servoshell.exe" SourceFile="${windowize(exe_path)}\servoshell.exe"/>
    </Package>
</Wix>
<%!
import os
import os.path as path
import re
import uuid
from servo.platform import host_triple

def make_id(s):
    s = s.replace("-", "_").replace("/", "_").replace("\\", "_")
    return "Id{}".format(s)

def listfiles(directory):
    return [f for f in os.listdir(directory)
            if path.isfile(path.join(directory, f))]

def listdirs(directory):
    return [f for f in os.listdir(directory)
            if path.isdir(path.join(directory, f))]

def listdeps(temp_dir):
    return [path.join(temp_dir, f) for f in os.listdir(temp_dir) if os.path.isfile(path.join(temp_dir, f)) and f != "servoshell.exe"]

def windowize(p):
    if not p.startswith("/"):
        return p
    return re.sub("^/([^/])+", "\\1:", p)

components = []
%>

<%def name="include_dependencies()">
% for f in listdeps(dir_to_temp):
              <File Id="${make_id(path.basename(f)).replace(".","").replace("+","x")}"
                    Name="${path.basename(f)}"
                    Source="${f}"
                    DiskId="1"/>
% endfor
</%def>

<%def name="include_directory(d, n)">
<Directory Id="${make_id(path.basename(d))}" Name="${n}">
  <Component Id="${make_id(path.basename(d))}"
             Guid="${uuid.uuid4()}"
             Bitness="always64">
    <CreateFolder/>
    <% components.append(make_id(path.basename(d))) %>
    % for f in listfiles(d):
    <File Id="${make_id(path.join(d, f).replace(dir_to_temp, ""))}"
          Name="${f}"
          Source="${windowize(path.join(d, f))}"
          DiskId="1"/>
    % endfor
  </Component>

  % for f in listdirs(d):
  ${include_directory(path.join(d, f), f)}
  % endfor
</Directory>
</%def>
