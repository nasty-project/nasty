# Kent Overstreet's linuxquota fork with bcachefs project quota support.
#
# The mainline quota-tools (4.11) do not recognise bcachefs as a quota-capable
# filesystem. This fork adds:
#   - hasbcachefsquota(): detects bcachefs quota state via Q_XFS_GETQSTAT,
#     handles colon-separated multi-device strings from /proc/mounts
#   - setproject: sets project IDs on directories via FS_IOC_FSSETXATTR +
#     BCHFS_IOC_REINHERIT_ATTRS for efficient bcachefs-native inheritance
#   - setquota -P / repquota -P: project quota limit management
#
# Source: http://evilpiepirate.org/git/linuxquota.git
# Used by NASty to enforce per-subvolume size limits via bcachefs project quotas.

{ pkgs, ... }:

let
  linuxquota = pkgs.stdenv.mkDerivation {
    pname = "linuxquota";
    version = "4.09-bcachefs";

    src = pkgs.fetchgit {
      url = "http://evilpiepirate.org/git/linuxquota.git";
      rev = "4bff1c34db3c37c4875454b3c647eaa933fee9c9";
      hash = "sha256-fqhsFsDRhW9s1KtxuDnK2zw8yaK8pCF30ujCsQEC5uQ=";
    };

    nativeBuildInputs = with pkgs; [
      autoconf
      automake
      libtool
      pkg-config
    ];

    # ext2direct support requires e2fsprogs dev headers; disable it to keep
    # the build simple — NASty only uses bcachefs.
    configureFlags = [
      "--disable-ldap"
      "--disable-ext2direct"
    ];

    preConfigure = ''
      ./autogen.sh
    '';

    meta = {
      description = "Linux quota tools with bcachefs project quota support";
      homepage = "http://evilpiepirate.org/git/linuxquota.git";
      license = pkgs.lib.licenses.gpl2Only;
      platforms = pkgs.lib.platforms.linux;
    };
  };

in {
  environment.systemPackages = [ linuxquota ];
}
