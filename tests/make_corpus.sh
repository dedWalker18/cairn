#!/usr/bin/env bash
# Build a synthetic home directory that exercises the scenarios in
# docs/SCENARIOS.md. Usage: make_corpus.sh <dest> [images-dir]
# Never point <dest> at a real directory: it is wiped first.
set -euo pipefail
dest=${1:?dest}; imgs=${2:-}
case "$dest" in /|/home|/home/*/|"$HOME"|"$HOME/") echo "refusing to use $dest"; exit 1;; esac
[ -d "$dest" ] && chmod -R u+rwx "$dest"
rm -rf "$dest"; mkdir -p "$dest"; cd "$dest"

mkdir -p "Pictures/dump" "Pictures/Manali trek Dec 25" "Zed/infra-tools" "Zed/acme-portal/docs" \
         "Documents/Finance" "Documents/Personal" "node_modules/junk" "build-cache" "Games/saves/EldenRing" \
         ".config/secret-app" "Unfinished/robot-arm"

if [ -n "$imgs" ]; then
  n=1; for f in "$imgs"/*.jpg; do cp "$f" "Pictures/dump/IMG_$(printf %04d $n).jpg"; n=$((n+1)); done
  cp "$imgs/Pizza.jpg" "Pictures/Manali trek Dec 25/DSC_0042.jpg"
  mkdir -p Downloads && cp "$imgs/Beach.jpg" Downloads/IMG_9001.jpg
  # photos "from December last year": capture-less files dated by mtime
  touch -d "2025-12-14 10:00" Pictures/dump/IMG_0001.jpg Pictures/dump/IMG_0005.jpg
  touch -d "2026-06-01 10:00" Pictures/dump/IMG_0002.jpg
fi
cat > "Pictures/Manali trek Dec 25/notes.txt" <<'T'
Trip to Manali with college friends, snow trek to Hampta pass, December 2025.
T

cat > Zed/infra-tools/deploy.sh <<'T'
#!/usr/bin/env bash
# Provision the staging VPC and EKS cluster on AWS.
set -euo pipefail
cd "$(dirname "$0")/terraform"
terraform init -upgrade
terraform plan -out tf.plan -var "region=us-east-1"
terraform apply -auto-approve tf.plan
T
cat > Zed/infra-tools/utils.py <<'T'
import json

def load_config(path):
    """Parse the YAML-ish settings file and return a dict of options."""
    with open(path) as f:
        return json.load(f)


def retry(fn, attempts=3):
    for i in range(attempts):
        try:
            return fn()
        except Exception:
            if i == attempts - 1:
                raise
T
printf 'Small helper scripts for infrastructure.\n' > Zed/infra-tools/README.md

cat > Zed/acme-portal/README.md <<'T'
# Acme portal
Customer portal built for Acme Corp (client). The client supplied their
standard operating procedure; see docs/.
T
python3 -I - <<'PY'
import zipfile
def docx(path, paras):
    body = "".join(f"<w:p><w:r><w:t>{p}</w:t></w:r></w:p>" for p in paras)
    xml = f'<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>'
    with zipfile.ZipFile(path, "w") as z:
        z.writestr("[Content_Types].xml", "<Types/>")
        z.writestr("word/document.xml", xml)
def xlsx(path, strings, sheet):
    ss = "".join(f"<si><t>{s}</t></si>" for s in strings)
    with zipfile.ZipFile(path, "w") as z:
        z.writestr("xl/workbook.xml", f'<workbook><sheets><sheet name="{sheet}" sheetId="1"/></sheets></workbook>')
        z.writestr("xl/sharedStrings.xml", f"<sst>{ss}</sst>")
docx("Zed/acme-portal/docs/Acme_SOP_v2.docx", ["Acme Corp Standard Operating Procedure", "1. Escalate production incidents to the on-call lead within 15 minutes.", "2. All releases require a signed change request."])
docx("Documents/Personal/cover_letter.docx", ["Dear hiring manager, I am applying for the role of embedded engineer."])
xlsx("Documents/Finance/sales_2025_q4.xlsx", ["Region", "Revenue", "North", "South", "Q4 2025 sales summary"], "Q4 Sales")
xlsx("Documents/Finance/sales_2025_q2.xlsx", ["Region", "Revenue", "Q2 2025 sales summary"], "Q2 Sales")
PY
touch -d "2026-01-10" Documents/Finance/sales_2025_q4.xlsx

printf 'stuff' > Games/saves/EldenRing/ER0000.sl2
printf 'Notes: inverse kinematics for 4-DOF robot arm, servo calibration TODO\n' > Unfinished/robot-arm/plan.md
printf 'module.exports = 1\n' > node_modules/junk/index.js
printf 'Signature: 8a477f597d28d172789f06886806bc55\n' > build-cache/CACHEDIR.TAG
printf 'should never be indexed\n' > build-cache/artifact.txt
printf 'AWS_SECRET_ACCESS_KEY=SECRET_SHOULD_NOT_APPEAR\n' > Zed/infra-tools/.env
printf 'PASSWORD=SECRET_SHOULD_NOT_APPEAR\n' > Documents/db-passwords.env
printf 'hidden\n' > .config/secret-app/config.txt
# hostile entries: non-UTF-8 name, FIFO, symlink loop, unreadable dir
printf 'café notes about espresso\n' > "$(printf 'Documents/caf\xe9.txt')"
mkfifo Documents/pipe.fifo
ln -s .. Documents/loop
mkdir -p Documents/locked && printf 'x\n' > Documents/locked/inside.txt && chmod 000 Documents/locked
echo "corpus ready at $dest"
