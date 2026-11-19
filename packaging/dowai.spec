Name:           dowai
Version:        %{package_version}
Release:        1%{?dist}
Summary:        DOWAI data pipeline CLI
License:        Apache-2.0
BuildArch:      x86_64

%description
DOWAI command-line executable.

%install
install -Dm755 %{binary} %{buildroot}/usr/bin/dowai

%files
/usr/bin/dowai
