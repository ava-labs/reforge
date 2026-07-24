// Copyright (C) 2026, Ava Labs, Inc. All rights reserved.
// See the file LICENSE for licensing terms.

pragma solidity 0.8.30;
/** @notice A Dummy struct
* flag: A boolean
* id: A 32 byte identifier
* #[derive(get_id_or_revert(contract=DummyLibrary))]
*/
struct Dummy {
    bool flag;
    uint32 ID;
}

// #[derive(get_id_or_revert(contract=DummyLibrary))]
struct WrappedBytes {
    bytes inner;
}

// #[derive(promote)]
library DummyLibrary {
    // #[derive(public)]
    function sayHello() private pure returns (string memory) {
        string memory name = printDummy();
        return string.concat("Hello ", name);
    }
}