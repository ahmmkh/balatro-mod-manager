# bmi-server on kh-kube

Everything lives in namespace `bmi`. Never touch `default`, `guident-test` or `kube-*`.
All commands run through `ssh kh@kh-kube 'sudo -n microk8s kubectl …'`.

## Apply order

1. Host volume (once, needs approval): see "Fixed-size data volume" below.
2. `namespace.yaml`, then `local-pv.yaml` (PV + PVC).
3. Secrets (never put values in files):
   ```sh
   openssl rand -hex 20 | ssh kh@kh-kube 'sudo -n microk8s kubectl -n bmi create secret generic bmi-env --from-file=ADMIN_TOKEN=/dev/stdin'
   # tunnel token: create the tunnel first (plan Task 11), pipe the token the same way:
   ssh kh@kh-kube 'sudo -n microk8s kubectl -n bmi create secret generic cloudflared-token --from-file=TUNNEL_TOKEN=/dev/stdin' < token-file
   ```
4. Image: `docker buildx build --platform linux/amd64 --load -f crates/bmi-server/Dockerfile -t bmi-server:<sha> . && docker save bmi-server:<sha> | ssh kh@kh-kube 'sudo -n microk8s ctr image import -'`; set the same tag in `bmi-server.yaml` and `backup-cronjob.yaml`.
5. `bmi-server.yaml`, `backup-cronjob.yaml`, `cloudflared.yaml`.

## Fixed-size data volume

The cluster has no StorageClass and hostpath volumes are unquota'd directories on the shared root
disk, so the data lives on a loop-mounted 8 GiB ext4 image. A full volume breaks only this service.

```sh
sudo cp /etc/fstab /etc/fstab.bak.bmi
sudo fallocate -l 8G /srv/bmi.img && sudo mkfs.ext4 -q -L bmi /srv/bmi.img && sudo mkdir -p /srv/bmi
echo '/srv/bmi.img /srv/bmi ext4 loop,nofail,noatime 0 0' | sudo tee -a /etc/fstab
sudo mount /srv/bmi && sudo chown 10001:10001 /srv/bmi
```
Undo: `sudo umount /srv/bmi`, delete the fstab line (backup at `/etc/fstab.bak.bmi`), delete `/srv/bmi.img`.

## Restore

The nightly CronJob writes `/data/backups/bmi-YYYY-MM-DD.sqlite` (counts database only, 7 kept).
Scale to 0, copy the chosen snapshot over `/data/bmi.sqlite` (delete `bmi.sqlite-wal`/`-shm`), scale to 1.
Download counts since the snapshot are lost; reports are not backed up (30-day retention).

## Rollback

`kubectl -n bmi rollout undo deploy/bmi-server`, or re-import the previous image tag and set it in the manifest.
The app keeps working from its cached catalog while the backend is down.
