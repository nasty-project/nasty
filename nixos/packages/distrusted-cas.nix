# emSign roots whose intermediates' private keys leaked (CVE-2026-111943,
# Mozilla bug 2083449). nixpkgs master (cacert 3.131.1) and Alpine
# ca-certificates 20261020 drop exactly these; nixos-26.05 still ships 3.126.
# Remove this list once the pinned nixpkgs carries cacert >= 3.131.
[
  "emSign ECC Root CA - G3"
  "emSign ECC Root CA - C3"
  "emSign Root CA - C1"
]
