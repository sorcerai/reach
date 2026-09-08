import importlib.util
import shutil
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "build_microvm_image.py"
spec = importlib.util.spec_from_file_location("build_microvm_image", SCRIPT)
image_builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(image_builder)


def test_image_builder_refuses_existing_and_dangling_output_paths(tmp_path):
    existing = tmp_path / "existing.ext4"
    existing.write_bytes(b"preserve this image")
    dangling = tmp_path / "dangling.ext4"
    destination = tmp_path / "unrelated.ext4"
    dangling.symlink_to(destination)

    for output in (existing, dangling):
        with pytest.raises(FileExistsError):
            image_builder.validate_output(output, tmp_path)

    assert existing.read_bytes() == b"preserve this image"
    assert dangling.is_symlink()
    assert not destination.exists()


def test_image_builder_rejects_outputs_outside_its_private_root(tmp_path):
    root = tmp_path / "owned"
    root.mkdir(mode=0o700)
    outside = tmp_path / "outside"
    outside.mkdir()
    (root / "redirect").symlink_to(outside, target_is_directory=True)

    for output in (root / "../outside/image.ext4", root / "redirect/image.ext4"):
        with pytest.raises(ValueError, match="outside"):
            image_builder.validate_output(output, root)

    assert not (outside / "image.ext4").exists()


def test_unmount_all_attempts_every_mount_and_retains_failures(monkeypatch):
    mounted = [Path("proc"), Path("sys"), Path("dev/pts")]
    attempted = []

    def fake_run(command):
        attempted.append(command[-1])
        if command[-1] == mounted[1]:
            raise OSError("busy")

    monkeypatch.setattr(image_builder, "run", fake_run)
    failures = image_builder._unmount_all(mounted)
    assert attempted == [Path("dev/pts"), Path("sys"), Path("proc")]
    assert len(failures) == 1
    assert mounted == [Path("sys")]


def test_staging_directory_is_preserved_when_mounts_remain(tmp_path):
    staging = image_builder._StagingDirectory(tmp_path)
    staging.live_mounts.append(Path("live"))
    with staging as path:
        path.joinpath("root").mkdir()
    assert path.exists()
    shutil.rmtree(path)
