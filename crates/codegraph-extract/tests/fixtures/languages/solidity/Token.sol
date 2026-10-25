// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./interfaces/IERC20.sol";
import {Ownable, Context} from "../access/Ownable.sol";
import * as Math from './utils/Math.sol';

type Price is uint256;

uint256 constant MAX_SUPPLY = 1_000_000 ether;

/// Free function at file scope.
function toPrice(uint256 raw) pure returns (Price) {
    return Price.wrap(raw);
}

error Unauthorized(address caller);

/**
 * @title A token with an owner.
 */
contract Token is IERC20, Ownable {
    using Math for uint256;

    struct Account {
        uint256 balance;
        mapping(address => uint256) allowances;
    }

    enum State { Active, Paused, Closed }

    event Transfer(address indexed from, address indexed to, uint256 value);
    error InsufficientBalance(uint256 available, uint256 required);

    mapping(address => Account) private accounts;
    uint256 public totalSupply;
    State internal state = State.Active;
    address public immutable deployer = msg.sender;

    /// Only while active.
    modifier whenActive() {
        require(state == State.Active, "paused");
        _;
    }

    constructor(uint256 supply) Ownable(msg.sender) {
        accounts[msg.sender].balance = supply;
        emit Transfer(address(0), msg.sender, supply);
    }

    /// Moves tokens.
    function transfer(address to, uint256 amount) external whenActive onlyOwner returns (bool) {
        Account storage from = accounts[msg.sender];
        if (from.balance < amount) revert InsufficientBalance(from.balance, amount);
        from.balance -= amount;
        accounts[to].balance += amount.mulDiv(1, 1);
        emit Transfer(msg.sender, to, amount);
        _afterTransfer(to);
        this.balanceOf(to);
        super.renounceOwnership();
        IERC20(to).totalSupply();
        return true;
    }

    function balanceOf(address who) public view returns (uint256) {
        return accounts[who].balance;
    }

    function _afterTransfer(address to) internal virtual {
        Vault v = new Vault(to);
        v.deposit{value: 0}();
        revert("unreachable");
    }

    fallback() external payable {}

    receive() external payable {
        revert Unauthorized(msg.sender);
    }
}
