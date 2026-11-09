<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <!-- Stillwater: a per-user install (no admin prompt), like Chrome's and Zen's default
       installs, so the auto-updater can upgrade it silently. -->
  <Package Name="Stillwater"
           Manufacturer="Stillwater contributors"
           UpgradeCode="603d88ba-b9d4-4932-8667-ecdfc2b94e0e"
           Language="1033"
           Codepage="1252"
           Version="0.8.3"
           Scope="perUser"
           InstallerVersion="500">
    <SummaryInformation Keywords="Installer"
                        Description="Stillwater Installer"
                        Manufacturer="Stillwater contributors"/>
    <MajorUpgrade AllowDowngrades="yes"/>
    <Media Id="1"
           Cabinet="Stillwater.cab"
           EmbedCab="yes"/>
    <Property Id="ARPPRODUCTICON" Value="StillwaterIcon.exe"/>
    <Property Id="ARPURLINFOABOUT" Value="https://github.com/WhoaThatCombo/stillwater"/>
    <Property Id="ARPHELPLINK" Value="https://github.com/WhoaThatCombo/stillwater/issues"/>

    <StandardDirectory Id="LocalAppDataFolder">
      <Directory Id="ProgramsDir" Name="Programs">
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
              <RemoveFolder Id="RemoveProgramsDir" Directory="ProgramsDir" On="uninstall"/>
            </Component>

            <!-- Register Stillwater as a web browser so it appears in Windows' Default apps.
                 Windows only lets the user pick the default; installers can't set it. -->
            <Component Id="BrowserRegistration" Guid="07ae4df0-bf73-4ef5-9cd7-d73eb060facc" Bitness="always64">
              <RegistryKey Root="HKCU" Key="Software\Clients\StartMenuInternet\Stillwater">
                <RegistryValue Type="string" Value="Stillwater" KeyPath="yes"/>
                <RegistryValue Key="DefaultIcon" Type="string" Value="[INSTALLDIR]Stillwater.exe,0"/>
                <RegistryValue Key="shell\open\command" Type="string" Value="&quot;[INSTALLDIR]Stillwater.exe&quot;"/>
                <RegistryValue Key="Capabilities" Name="ApplicationName" Type="string" Value="Stillwater"/>
                <RegistryValue Key="Capabilities" Name="ApplicationDescription" Type="string" Value="A calm, lightweight browser built on Servo."/>
                <RegistryValue Key="Capabilities" Name="ApplicationIcon" Type="string" Value="[INSTALLDIR]Stillwater.exe,0"/>
                <RegistryValue Key="Capabilities\StartMenu" Name="StartMenuInternet" Type="string" Value="Stillwater"/>
                <RegistryValue Key="Capabilities\URLAssociations" Name="http" Type="string" Value="StillwaterURL"/>
                <RegistryValue Key="Capabilities\URLAssociations" Name="https" Type="string" Value="StillwaterURL"/>
                <RegistryValue Key="Capabilities\FileAssociations" Name=".htm" Type="string" Value="StillwaterHTML"/>
                <RegistryValue Key="Capabilities\FileAssociations" Name=".html" Type="string" Value="StillwaterHTML"/>
                <RegistryValue Key="Capabilities\FileAssociations" Name=".xhtml" Type="string" Value="StillwaterHTML"/>
              </RegistryKey>
              <RegistryValue Root="HKCU" Key="Software\RegisteredApplications" Name="Stillwater" Type="string"
                             Value="Software\Clients\StartMenuInternet\Stillwater\Capabilities"/>
              <RegistryKey Root="HKCU" Key="Software\Classes\StillwaterURL">
                <RegistryValue Type="string" Value="Stillwater URL"/>
                <RegistryValue Name="URL Protocol" Type="string" Value=""/>
                <RegistryValue Key="DefaultIcon" Type="string" Value="[INSTALLDIR]Stillwater.exe,0"/>
                <RegistryValue Key="shell\open\command" Type="string" Value="&quot;[INSTALLDIR]Stillwater.exe&quot; -- &quot;%1&quot;"/>
              </RegistryKey>
              <RegistryKey Root="HKCU" Key="Software\Classes\StillwaterHTML">
                <RegistryValue Type="string" Value="Stillwater HTML Document"/>
                <RegistryValue Key="DefaultIcon" Type="string" Value="[INSTALLDIR]Stillwater.exe,0"/>
                <RegistryValue Key="shell\open\command" Type="string" Value="&quot;[INSTALLDIR]Stillwater.exe&quot; -- &quot;%1&quot;"/>
              </RegistryKey>
              <RegistryValue Root="HKCU" Key="Software\Stillwater" Name="InstallDir" Type="string" Value="[INSTALLDIR]"/>
            </Component>

            ${include_directory(resources_path, "resources")}
          </Directory>
        </Directory>
      </StandardDirectory>

      <StandardDirectory Id="ProgramMenuFolder">
        <Component Id="StartMenuShortcut" Guid="8ac2a40a-a02b-45d0-bc2d-31f7ec1877fb">
          <Shortcut Id="StartMenuStillwater"
            Name="Stillwater"
            Description="A calm, lightweight browser built on Servo"
            Target="[INSTALLDIR]Stillwater.exe"
            WorkingDirectory="INSTALLDIR"
            Icon="StillwaterIcon.exe"/>
          <RegistryValue Root="HKCU" Key="Software\Stillwater" Name="StartMenuShortcut" Type="integer" Value="1" KeyPath="yes"/>
        </Component>
      </StandardDirectory>

      <StandardDirectory Id="DesktopFolder">
        <Component Id="DesktopShortcut" Guid="70114373-2e7e-46ed-b634-fb7984eb6d3f">
          <Shortcut Id="DesktopStillwater"
            Name="Stillwater"
            Target="[INSTALLDIR]Stillwater.exe"
            WorkingDirectory="INSTALLDIR"
            Icon="StillwaterIcon.exe"/>
          <RegistryValue Root="HKCU" Key="Software\Stillwater" Name="DesktopShortcut" Type="integer" Value="1" KeyPath="yes"/>
        </Component>
      </StandardDirectory>

      <Feature Id="Complete" Level="1">
        <ComponentRef Id="StillwaterMain"/>
        <ComponentRef Id="BrowserRegistration"/>
         % for c in components:
         <ComponentRef Id="${c}"/>
         % endfor
        <ComponentRef Id="StartMenuShortcut"/>
        <ComponentRef Id="DesktopShortcut"/>
      </Feature>

      <Icon Id="StillwaterIcon.exe" SourceFile="${windowize(exe_path)}\servoshell.exe"/>
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
