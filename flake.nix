{
  description = "ntfs-reader development shell with native Rust and Windows MSVC cross-build tooling";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        isX86_64 = pkgs.stdenv.hostPlatform.isx86_64;
        rustToolchain = pkgs.rust-bin.stable."1.98.1".default.override {
          extensions = [ "clippy" "rustfmt" ];
          targets = [
            "x86_64-pc-windows-msvc"
            "i686-pc-windows-msvc"
          ];
        };
        windowsVm = pkgs.writeShellApplication {
          name = "ntfs-windows-vm";
          runtimeInputs = with pkgs; [
            coreutils
            qemu_kvm
            socat
            swtpm
            tigervnc
          ];
          text = ''
            vm_dir="''${NTFS_READER_VM_DIR:-''${XDG_DATA_HOME:-$HOME/.local/share}/windows-vm}"
            pid_file="$vm_dir/qemu.pid"
            monitor_socket="$vm_dir/monitor.sock"
            swtpm_socket="$vm_dir/swtpm.sock"
            swtpm_pid_file="$vm_dir/swtpm.pid"
            stress_disk="''${NTFS_READER_STRESS_DISK:-$vm_dir/ntfs-stress.qcow2}"
            firmware=${pkgs.qemu_kvm}/share/qemu/edk2-x86_64-secure-code.fd

            is_running() {
              [[ -r "$pid_file" ]] && kill -0 "$(<"$pid_file")" 2>/dev/null
            }

            require_file() {
              if [[ ! -e "$1" ]]; then
                echo "Missing VM file: $1" >&2
                exit 1
              fi
            }

            start() {
              if is_running; then
                echo "The Windows VM is already running with PID $(<"$pid_file")"
                return
              fi

              require_file "$vm_dir/windows-system.qcow2"
              require_file "$vm_dir/ntfs-test.qcow2"
              require_file "$vm_dir/windows-11-enterprise-25h2-eval-x64.iso"
              require_file "$vm_dir/uefi-vars.fd"
              require_file "$vm_dir/payload"

              # The stress disk (`S:` in the guest, see tools/vm/stress.ps1 in the dev docs) is thin
              # and created on first start. `NTFS_READER_STRESS_DISK` moves it (for example onto an
              # external SSD); a directory that is not there means the drive is not mounted, and the
              # disk is never created anywhere else. Deleting the file is safe: the next start makes
              # a new one and the guest rebuilds the fixture on it.
              if [[ ! -e "$stress_disk" ]]; then
                if [[ ! -d "$(dirname "$stress_disk")" ]]; then
                  echo "The stress disk directory does not exist (is the drive mounted?): $(dirname "$stress_disk")" >&2
                  exit 1
                fi
                qemu-img create -f qcow2 "$stress_disk" 2T
              fi

              mkdir -p "$vm_dir/tpm"
              rm -f "$pid_file" "$monitor_socket" "$swtpm_socket" "$swtpm_pid_file"
              chmod u+w "$vm_dir/uefi-vars.fd"

              swtpm socket \
                --tpm2 \
                --tpmstate dir="$vm_dir/tpm" \
                --ctrl type=unixio,path="$swtpm_socket" \
                --pid file="$swtpm_pid_file" \
                --terminate \
                --daemon

              if ! qemu-system-x86_64 \
                -name ntfs-reader-windows \
                -machine q35,accel=kvm,smm=on \
                -cpu host,hv_relaxed=on,hv_vapic=on,hv_time=on \
                -smp 8 \
                -m 8G \
                -rtc base=localtime \
                -global driver=cfi.pflash01,property=secure,value=on \
                -drive if=pflash,format=raw,readonly=on,file="$firmware" \
                -drive if=pflash,format=raw,file="$vm_dir/uefi-vars.fd" \
                -chardev socket,id=chrtpm,path="$swtpm_socket" \
                -tpmdev emulator,id=tpm0,chardev=chrtpm \
                -device tpm-crb,tpmdev=tpm0 \
                -device qemu-xhci,id=xhci \
                -drive if=none,id=system,format=qcow2,file="$vm_dir/windows-system.qcow2",discard=unmap \
                -device nvme,drive=system,serial=NTFSSYSTEM,bootindex=2 \
                -drive if=none,id=test,format=qcow2,file="$vm_dir/ntfs-test.qcow2",discard=unmap \
                -device nvme,drive=test,serial=NTFSTEST \
                -drive if=none,id=stress,format=qcow2,file="$stress_disk",discard=unmap \
                -device nvme,drive=stress,serial=NTFSSTRESS \
                -drive if=none,id=installer,media=cdrom,readonly=on,file="$vm_dir/windows-11-enterprise-25h2-eval-x64.iso" \
                -device ide-cd,drive=installer,bootindex=1 \
                -drive if=none,id=payload,format=raw,file=fat:rw:"$vm_dir/payload" \
                -device usb-storage,drive=payload,removable=on \
                -netdev user,id=net \
                -device e1000e,netdev=net \
                -display none \
                -vnc 127.0.0.1:1 \
                -monitor unix:"$monitor_socket",server=on,wait=off \
                -pidfile "$pid_file" \
                -daemonize; then
                if [[ -r "$swtpm_pid_file" ]]; then
                  kill "$(<"$swtpm_pid_file")" 2>/dev/null || true
                fi
                exit 1
              fi

              echo "Windows VM started. Run 'ntfs-windows-vm view' to connect."
            }

            case "''${1:-start}" in
              start)
                start
                ;;
              status)
                if is_running; then
                  echo "The Windows VM is running with PID $(<"$pid_file")"
                else
                  echo "The Windows VM is stopped"
                  exit 1
                fi
                ;;
              stop)
                if is_running; then
                  printf 'system_powerdown\n' | socat - UNIX-CONNECT:"$monitor_socket"
                  echo "Requested a graceful Windows shutdown."
                else
                  echo "The Windows VM is already stopped."
                fi
                ;;
              view)
                vncviewer 127.0.0.1:5901
                ;;
              *)
                echo "Usage: ntfs-windows-vm {start|status|stop|view}" >&2
                exit 2
                ;;
            esac
          '';
        };
      in {
        devShells.default = pkgs.mkShell {
          packages = (with pkgs; [
              rustToolchain
              cargo-xwin
              clang
              llvm
              lld
            ])
            ++ pkgs.lib.optional isX86_64 windowsVm;

          shellHook = ''
            echo "ntfs-reader dev shell"
            echo "  lint:       cargo fmt --check && cargo clippy"
            echo "  MSVC build: cargo xwin build --target x86_64-pc-windows-msvc"
            echo "  MSVC tests: cargo xwin test --no-run --target x86_64-pc-windows-msvc"
            ${pkgs.lib.optionalString isX86_64 ''echo "  Windows VM: ntfs-windows-vm start"''}
          '';
        };
      } // pkgs.lib.optionalAttrs isX86_64 {
        packages.windows-vm = windowsVm;
        apps.windows-vm = (flake-utils.lib.mkApp { drv = windowsVm; }) // {
          meta.description = "Run the ntfs-reader Windows test VM";
        };
      });
}
