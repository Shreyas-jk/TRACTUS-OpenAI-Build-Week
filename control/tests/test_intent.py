from types import SimpleNamespace

import pytest

from control.intent import ARTIFACT_PATHS, ContractSpec, Operation, extract_intent


class FakeResponses:
    def __init__(self, contract: ContractSpec) -> None:
        self.contract = contract
        self.arguments = None

    async def parse(self, **kwargs):
        self.arguments = kwargs
        return SimpleNamespace(output_parsed=self.contract)


class FakeClient:
    def __init__(self, contract: ContractSpec) -> None:
        self.responses = FakeResponses(contract)


@pytest.mark.asyncio
async def test_extraction_enforces_artifacts_and_implied_operations() -> None:
    client = FakeClient(
        ContractSpec(
            task="Fix the API handler",
            allowed_paths=[
                "src/api/**",
                "target/",
                "target/**",
                "node_modules",
                "node_modules/**",
                "**/__pycache__/",
                "**/__pycache__/**",
                ".venv/",
                ".venv/**",
                "/test/api",
                "test/api/**",
            ],
            allowed_ops=[Operation.READ, Operation.EDIT, Operation.RUN],
            deps_may_change=True,
            network=True,
        )
    )

    contract = await extract_intent("Fix the API handler in src/api.", client=client)

    assert contract.allowed_paths == [
        "src/api/**",
        "target/**",
        "node_modules/**",
        "**/__pycache__/**",
        ".venv/**",
        "test/api/**",
    ]
    assert all(path in contract.allowed_paths for path in ARTIFACT_PATHS)
    assert "target" not in contract.allowed_paths
    assert "target/" not in contract.allowed_paths
    assert len(contract.allowed_paths) == len(set(contract.allowed_paths))
    assert Operation.TEST in contract.allowed_ops
    assert Operation.BUILD in contract.allowed_ops
    assert Operation.RUN not in contract.allowed_ops
    assert contract.network is False
    assert contract.deps_may_change is False
    assert "NEVER emit build-artifact directories" in client.responses.arguments["instructions"]
    assert client.responses.arguments["text_format"] is ContractSpec
