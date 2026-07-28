"""Named template configurations — compose kernel + rootfs + layers.

Templates are stored as JSON files under the managed templates directory
so they survive across sessions and can be referenced by name in any
Terrarium client.

Example::

    from terra.template import Template

    # Define a template from existing layers:
    t = Template.from_layers("alpine", ["python312", "base"],
                              kernel="k612", name="py312")

    # List all saved templates:
    names = Template.list()

    # Load and inspect:
    t = Template.load("py312")
    print(t.base, t.layers, t.kernel)

    # Build a layer by configuring inside a builder VM:
    def setup_python(client, builder_name):
        client.vm_exec(builder_name, ["apk", "add", "python3"])

    Template.build("py312", setup_python)

    # Remove a template:
    Template.remove("py312")
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, ClassVar

from . import images, paths
from .client import TerraClient, TerraError


@dataclass
class Template:
    """A named composition of kernel + base distro + tool layers.

    Attributes:
        name: Unique template name (used as the JSON filename stem).
        base: Base distro layer name — ``"alpine"`` or ``"ubuntu"``.
        layers: Ordered tool layers (highest priority first).
        kernel: Kernel variant name (optional; ``None`` means default).
    """

    name: str
    base: str
    layers: list[str] = field(default_factory=list)
    kernel: str | None = None

    # Not a dataclass field — computed lazily so it respects $TERRA_HOME.
    _TEMPLATE_DIR: ClassVar[Path | None] = None

    @staticmethod
    def _template_dir() -> Path:
        """Return the managed templates directory (respects *TERRA_HOME*)."""
        if Template._TEMPLATE_DIR is None:
            Template._TEMPLATE_DIR = paths.root() / "templates"
        return Template._TEMPLATE_DIR

    def save(self) -> Path:
        """Persist this template to disk.

        Returns the path written so callers can verify.
        """
        d = self._template_dir()
        d.mkdir(parents=True, exist_ok=True)
        path = d / f"{self.name}.json"
        data = {
            "name": self.name,
            "base": self.base,
            "layers": self.layers,
            "kernel": self.kernel,
        }
        path.write_text(json.dumps(data, indent=2))
        return path

    # ------------------------------------------------------------------
    # classmethods — factory / collection / removal
    # ------------------------------------------------------------------

    @classmethod
    def from_layers(
        cls,
        base: str,
        layers: list[str],
        kernel: str | None = None,
        name: str | None = None,
    ) -> "Template":
        """Create a template from existing layers and persist it.

        If *name* is not given a reasonable default is derived from the
        base and layer list.

        Returns the new Template (already saved).
        """
        if name is None:
            name = f"{base}-{'-'.join(layers)}"
        t = cls(name=name, base=base, layers=list(layers), kernel=kernel)
        t.save()
        return t

    @classmethod
    def list(cls) -> list[str]:
        """Return the list of saved template names (JSON stems)."""
        d = cls._template_dir()
        if not d.exists():
            return []
        items: list[str] = []
        for f in sorted(d.glob("*.json")):
            items.append(f.stem)
        return items

    @classmethod
    def remove(cls, name: str) -> bool:
        """Remove a saved template by name.

        Returns ``True`` if the file was deleted, ``False`` if it did not
        exist (idempotent removal).
        """
        path = cls._template_dir() / f"{name}.json"
        if path.exists():
            path.unlink()
            return True
        return False

    @classmethod
    def load(cls, name: str) -> "Template":
        """Load a template from its JSON file.

        Raises :class:`FileNotFoundError` when the template does not exist.
        """
        path = cls._template_dir() / f"{name}.json"
        if not path.exists():
            raise FileNotFoundError(f"Template {name!r} not found")
        data = json.loads(path.read_text())
        return cls(**data)

    # ------------------------------------------------------------------
    # builder VM workflow
    # ------------------------------------------------------------------

    @classmethod
    def build(
        cls,
        name: str,
        builder_func: Callable[[TerraClient, str], None],
        *,
        client: TerraClient | None = None,
        timeout_secs: int = 600,
        no_net: bool = False,
    ) -> Path:
        """Build a layer by loading the template and configuring inside a
        builder VM.

        The workflow mirrors ``terra layer build``:

        1. Load the template *name*.
        2. Create a builder VM from the template's base layer with a
           persistent upperdir.
        3. Call *builder_func(client, builder_name)* — the user-provided
           function that configures the VM (install packages, run scripts).
        4. Destroy the builder VM.
        5. Pack the upperdir delta as an EROFS layer named after the
           template.

        Args:
            name: Template name (must exist).
            builder_func: Callable ``(client, builder_vm_name)`` that
                performs the configuration inside the builder VM.
            client: Optional pre-connected :class:`TerraClient`.
            timeout_secs: Per-command timeout for the user's setup step.
            no_net: If ``True``, the builder VM starts without networking.

        Returns:
            Path to the created EROFS layer image.

        Raises:
            FileNotFoundError: if the template does not exist.
            TerraError: on engine / VM failures.
        """
        template = cls.load(name)

        if client is None:
            client = TerraClient()

        builder = f"lb-{name}"

        # Resolve kernel — template may name a variant or be None (default).
        kernel_path: str
        if template.kernel:
            kernel_path = str(images.resolve_kernel(template.kernel))
        else:
            kernel_path = str(images.resolve_kernel("default"))

        # Map the base label to the actual distro system layer name.
        system_map = {"alpine": "base", "ubuntu": "ubuntu"}
        system = system_map.get(template.base)
        if system is None:
            raise ValueError(
                f"Unsupported base {template.base!r}; expected 'alpine' or 'ubuntu'"
            )

        # 1) Create the builder VM.
        try:
            client.vm_create(
                name=builder,
                kernel=kernel_path,
                initramfs=str(images.resolve_rootfs("virtiofs")),
                cpus=1,
                memory_mb=512,
                layers=[system],
                upper=builder,
                net=not no_net,
            )
        except TerraError:
            raise

        try:
            # 2) Run the user's configuration inside the VM.
            builder_func(client, builder)

            # 3) Cleanup runtime noise (mirrors cmd_image_layer_build).
            client.vm_exec(
                builder,
                [
                    "sh",
                    "-c",
                    "rm -rf /tmp/* /run/* /var/log/* /etc/resolv.conf 2>/dev/null; sync",
                ],
                timeout_secs=30,
            )
        finally:
            # 4) Always destroy the builder VM.
            try:
                client.vm_destroy(builder)
            except TerraError:
                pass

        # 5) Pack the upperdir delta into an EROFS layer.
        fs_root = os.environ.get("TERRA_STATE_DIR", "/tmp/terra-disks")
        upper_dir = Path(fs_root) / "fs" / "uppers" / builder
        if not upper_dir.is_dir():
            raise FileNotFoundError(
                f"upperdir {upper_dir} not found — "
                "Template.build needs a LOCAL daemon "
                "(the upperdir lives on the daemon host)"
            )

        return images.build_layer(str(upper_dir), name)
