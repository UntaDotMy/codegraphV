pragma solidity ^0.8.0;

interface IVault {
    function deposit() external payable;
    event Deposited(address who);
}

library SafeMath {
    function add(uint256 a, uint256 b) internal pure returns (uint256) {
        uint256 c = a + b;
        require(c >= a);
        return c;
    }
}

abstract contract Base {
    function hook() internal virtual;
}

contract Vault is Base, IVault {
    address public owner;
    uint256[] private history;

    constructor(address who) {
        owner = who;
    }

    function deposit() external payable override {
        history.push(SafeMath.add(msg.value, 0));
        emit IVault.Deposited(msg.sender);
        hook();
    }

    function hook() internal override {
        uint256[] memory copy = new uint256[](history.length);
        (bool ok, ) = owner.call{value: address(this).balance}("");
        require(ok);
        copy;
    }
}
