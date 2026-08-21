// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.

pragma solidity ^0.8.30;

import {Test} from "forge-std/Test.sol";
import {Dummy, WrappedBytes, DummyLibrary} from "../Sample.sol";

contract TestDummyLibrary is Test {
    DummyLibrary dummy;

    function setUp() public  {
        dummy = new DummyLibrary();
    }

    function testSayHello() public view {
        string memory output = dummy.sayHello();
        assertEq(output, "Hello Dummy");
    }

    function testGetIDOrRevert() public {
        Dummy memory obj1 = Dummy ({flag: true, ID: uint32(10) });
        assertEq(dummy.getIdDummy(obj1), uint32(10));
        WrappedBytes memory obj2 = WrappedBytes({inner: "0xDEADBEEF"});
        vm.expectRevert("WrappedBytes has no field ID");
        dummy.getIdWrappedBytes(obj2);
    }
}