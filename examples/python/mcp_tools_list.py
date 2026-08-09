import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdks" / "python"))

from solo_client import SoloClient  # noqa: E402

solo = SoloClient(
    base_url=os.getenv("SOLO_URL", "http://127.0.0.1:17821"),
    bearer_token=os.getenv("SOLO_BEARER_TOKEN"),
)

session = solo.mcp_connect("solo-sdk-example", "0.0.0")

for tool in solo.mcp_list_tools(session):
    print(tool["name"])
