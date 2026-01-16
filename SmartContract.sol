// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/access/Ownable.sol";
import "@openzeppelin/contracts/utils/Pausable.sol";
import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import "@uniswap/v3-periphery/contracts/interfaces/ISwapRouter.sol";
import { IPoolAddressesProvider } from "@aave/core-v3/contracts/interfaces/IPoolAddressesProvider.sol";
import { IPool } from "@aave/core-v3/contracts/interfaces/IPool.sol";
import { IFlashLoanSimpleReceiver } from "@aave/core-v3/contracts/flashloan/interfaces/IFlashLoanSimpleReceiver.sol";

/// @title UltimateArbitrageBot V2 - Universal Token Compatibility + Maximum Gas Optimization
/// @notice Bot d'arbitrage compatible avec TOUS les tokens (fee-on-transfer, rebasing, non-standard, etc.)
/// @dev V2: Mapping DEX simplifié (uint8) + support natif Aerodrome Slipstream
/// @author Romain - Optimized Version V2
contract UltimateArbitrageBot is IFlashLoanSimpleReceiver, Ownable, Pausable, ReentrancyGuard {
    
    // ========== CUSTOM ERRORS (Gas Efficient) ==========
    error Unauthorized();
    error UnsupportedAsset();
    error Unprofitable();
    error DeadlineExpired();
    error BlockTooHigh();
    error ZeroAmount();
    error InvalidPath();
    error RateLimitExceeded();
    error InvalidDex();
    error TransferFailed();
    error InsufficientOutput();
    error SlippageExceeded();
    error InvalidCallback();
    error MEVProtectionFailed();

    // ========== STRUCTS (Packed for Gas) ==========
    
    /// @notice Structure pour définir un swap step
    /// @dev DEX mapping: 0=UniswapV2, 1=UniswapV3, 2=Aerodrome, 3=Curve, 4=Balancer, 5=Sushiswap, 6=AerodromeSlipstream
    struct SwapStep {
        uint8 dex;           // DEX ID (0-6)
        address tokenIn;
        address tokenOut;
        bytes data;          // Données spécifiques au DEX
        uint128 minOut;      // Slippage protection
    }

    struct FlashParams {
        SwapStep[] steps;
        uint128 minProfit;
        uint32 deadline;
        uint32 maxBlock;
        address profitToken;
        bool useBackrunProtection;
    }

    // ========== IMMUTABLES (Gas Savings) ==========
    ISwapRouter public immutable UNISWAP_V3_ROUTER;
    address public immutable UNISWAP_V2_ROUTER;
    address public immutable AERODROME_ROUTER;
    address public immutable AERODROME_SLIPSTREAM_ROUTER;
    address public immutable CURVE_ROUTER;
    address public immutable BALANCER_VAULT;
    address public immutable SUSHISWAP_ROUTER;
    IPool public immutable AAVE_POOL;

    // ========== STORAGE (Optimized Layout) ==========
    // Slot 0: packed variables
    uint88 public totalTrades;
    uint88 public tradesThisHour;
    uint48 public lastHourStart;
    uint32 public maxTradesPerHour;
    
    // Slot 1: Total profit tracking
    uint256 public totalProfitWei;
    
    // Slot 2: MEV & Priority
    uint128 public maxPriorityFeeGwei;
    uint128 public minProfitAfterGas;
    
    // Slot 3: Flash loan optimization
    address public preferredFlashLoanProvider;
    
    // Slot 4+: Mappings
    mapping(address => bool) public authorized;
    mapping(address => bool) public supportedAssets;
    mapping(address => uint256) public profitByToken;
    mapping(bytes32 => bool) public executedTxs;
    mapping(address => uint256) public lastExecutionBlock;

    // ========== EVENTS ==========
    event Executed(
        address indexed asset,
        uint256 profit,
        uint256 indexed tradeNumber,
        uint256 gasUsed,
        uint256 effectiveGasPrice
    );
    event SwapExecuted(
        uint8 indexed dex,
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        uint256 amountOut
    );
    event CallerUpdated(address indexed caller, bool status);
    event EmergencyWithdraw(address indexed token, uint256 amount);
    event MEVProtection(uint256 priorityFee, uint256 baseFee);
    event UnprofitableTrade(address asset, uint256 expectedProfit, uint256 actualProfit, uint256 gasCost);

    // ========== MODIFIERS ==========
    modifier onlyAuthorized() {
        if (!authorized[msg.sender]) revert Unauthorized();
        _;
    }

    modifier checkRateLimit() {
        unchecked {
            uint48 currentTime = uint48(block.timestamp);
            if (currentTime >= lastHourStart + 1 hours) {
                tradesThisHour = 0;
                lastHourStart = currentTime;
            }
            if (tradesThisHour >= maxTradesPerHour) revert RateLimitExceeded();
            ++tradesThisHour;
        }
        _;
    }
    
    modifier checkMEVProtection() {
        if (tx.gasprice > block.basefee) {
            uint256 priorityFee = tx.gasprice - block.basefee;
            if (priorityFee > uint256(maxPriorityFeeGwei) * 1 gwei) {
                revert MEVProtectionFailed();
            }
            emit MEVProtection(priorityFee, block.basefee);
        }
        _;
    }
    
    modifier antiSpam() {
        if (lastExecutionBlock[msg.sender] == block.number) revert RateLimitExceeded();
        lastExecutionBlock[msg.sender] = block.number;
        _;
    }

    // ========== CONSTRUCTOR ==========
    constructor(
        address _uniswapV3Router,
        address _uniswapV2Router,
        address _aerodromeRouter,
        address _aerodromeSlipstreamRouter,
        address _curveRouter,
        address _balancerVault,
        address _sushiswapRouter,
        address _aaveProvider
    ) Ownable(msg.sender) {
        UNISWAP_V3_ROUTER = ISwapRouter(_uniswapV3Router);
        UNISWAP_V2_ROUTER = _uniswapV2Router;
        AERODROME_ROUTER = _aerodromeRouter;
        AERODROME_SLIPSTREAM_ROUTER = _aerodromeSlipstreamRouter;
        CURVE_ROUTER = _curveRouter;
        BALANCER_VAULT = _balancerVault;
        SUSHISWAP_ROUTER = _sushiswapRouter;
        
        address pool = IPoolAddressesProvider(_aaveProvider).getPool();
        require(pool != address(0), "Invalid pool");
        AAVE_POOL = IPool(pool);

        authorized[msg.sender] = true;
        lastHourStart = uint48(block.timestamp);
        maxTradesPerHour = 100;
        maxPriorityFeeGwei = 2;
        minProfitAfterGas = 0.001 ether;
        preferredFlashLoanProvider = _aaveProvider;
    }

    // ========== MAIN ENTRY POINTS ==========
    
    /// @notice Exécute un arbitrage avec flash loan
    function execute(
        address asset,
        uint256 amount,
        SwapStep[] calldata steps,
        uint128 minProfit,
        uint32 deadline,
        uint32 maxBlock,
        uint256 nonce
    ) 
        external 
        onlyAuthorized
        whenNotPaused
        nonReentrant
        checkRateLimit
        checkMEVProtection
        antiSpam
    {
        // Anti-replay
        bytes32 txHash = keccak256(abi.encodePacked(asset, amount, nonce, msg.sender));
        if (executedTxs[txHash]) revert Unauthorized();
        executedTxs[txHash] = true;
        
        uint256 gasStart = gasleft();
        
        // Validations
        if (amount == 0) revert ZeroAmount();
        if (!supportedAssets[asset]) revert UnsupportedAsset();
        if (uint32(block.timestamp) > deadline) revert DeadlineExpired();
        if (maxBlock != 0 && uint32(block.number) > maxBlock) revert BlockTooHigh();
        if (steps.length == 0 || steps.length > 6) revert InvalidPath();

        bytes memory data = abi.encode(
            FlashParams({
                steps: steps,
                minProfit: minProfit,
                deadline: deadline,
                maxBlock: maxBlock,
                profitToken: asset,
                useBackrunProtection: true
            })
        );

        AAVE_POOL.flashLoanSimple(address(this), asset, amount, data, 0);
        
        unchecked {
            uint256 gasUsed = gasStart - gasleft();
            emit Executed(asset, 0, totalTrades, gasUsed, tx.gasprice);
        }
    }
    
    /// @notice Exécution atomique sans flash loan
    function executeAtomic(
        address asset,
        uint256 amount,
        SwapStep[] calldata steps,
        uint128 minProfit,
        uint32 deadline,
        uint256 nonce
    )
        external
        onlyAuthorized
        whenNotPaused
        nonReentrant
        checkRateLimit
        checkMEVProtection
        antiSpam
    {
        // Anti-replay
        bytes32 txHash = keccak256(abi.encodePacked(asset, amount, nonce, msg.sender, "atomic"));
        if (executedTxs[txHash]) revert Unauthorized();
        executedTxs[txHash] = true;
        
        uint256 gasStart = gasleft();
        
        // Validations
        if (amount == 0) revert ZeroAmount();
        if (!supportedAssets[asset]) revert UnsupportedAsset();
        if (uint32(block.timestamp) > deadline) revert DeadlineExpired();
        if (steps.length == 0 || steps.length > 6) revert InvalidPath();
        
        // Vérifier balance
        uint256 balance = _balanceOf(asset);
        if (balance < amount) revert InsufficientOutput();
        
        uint256 initialBalance = balance;
        uint256 currentAmount = amount;
        
        // Execute swaps
        unchecked {
            for (uint256 i; i < steps.length; ++i) {
                currentAmount = _executeSwapWithBalanceCheck(steps[i], currentAmount);
            }
        }
        
        // Vérifier profit
        uint256 finalBalance = _balanceOf(asset);
        if (finalBalance <= initialBalance + minProfit) revert Unprofitable();
        
        uint256 profit = finalBalance - initialBalance;
        
        // Vérifier coût gas
        uint256 gasUsed = gasStart - gasleft() + 50000;
        uint256 gasCost = gasUsed * tx.gasprice;
        
        if (profit < gasCost) {
            emit UnprofitableTrade(asset, profit, profit, gasCost);
            revert Unprofitable();
        }
        
        _updateMetrics(asset, profit);
        emit Executed(asset, profit, totalTrades, gasUsed, tx.gasprice);
    }

    // ========== AAVE CALLBACK ==========
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override returns (bool) {
        if (msg.sender != address(AAVE_POOL)) revert Unauthorized();
        if (initiator != address(this)) revert Unauthorized();

        FlashParams memory fp = abi.decode(params, (FlashParams));

        unchecked {
            if (uint32(block.timestamp) > fp.deadline) revert DeadlineExpired();
            if (fp.maxBlock != 0 && uint32(block.number) > fp.maxBlock) revert BlockTooHigh();
        }

        // Mesure balance réel (fee-on-transfer)
        uint256 actualAmount = _balanceOf(asset);
        uint256 currentAmount = actualAmount;

        // Execute swaps
        unchecked {
            for (uint256 i; i < fp.steps.length; ++i) {
                currentAmount = _executeSwapWithBalanceCheck(fp.steps[i], currentAmount);
            }
        }

        // Calculate profit
        uint256 finalBalance = _balanceOf(asset);
        uint256 repayment;
        uint256 profit;
        uint256 gasCostEstimate;
        
        unchecked {
            repayment = amount + premium;
            gasCostEstimate = 300000 * tx.gasprice;
            
            if (finalBalance < repayment) revert Unprofitable();
            
            uint256 grossProfit = finalBalance - repayment;
            
            if (grossProfit < fp.minProfit || grossProfit < gasCostEstimate) {
                emit UnprofitableTrade(asset, fp.minProfit, grossProfit, gasCostEstimate);
                revert Unprofitable();
            }
            
            profit = grossProfit;
        }

        // Approve & repay
        _universalApprove(asset, address(AAVE_POOL), repayment);

        if (profit > 0) {
            _universalTransfer(asset, owner(), profit);
            _updateMetrics(asset, profit);
        }

        return true;
    }

    // ========== INTERNAL SWAP LOGIC ==========
    
    /// @notice Execute swap avec mesure de balance réelle
    /// @dev DEX Mapping: 0=UniswapV2, 1=UniswapV3, 2=Aerodrome, 3=Curve, 4=Balancer, 5=Sushiswap, 6=AerodromeSlipstream
    function _executeSwapWithBalanceCheck(SwapStep memory step, uint256 amountIn) internal returns (uint256 amountOut) {
        address tokenOut = step.tokenOut;
        uint256 balanceBefore = _balanceOf(tokenOut);

        // ✅ DEX Routing avec uint8
        if (step.dex == 0) {
            // UNISWAP_V2
            _swapUniswapV2(step.tokenIn, step.tokenOut, amountIn, step.minOut);
        } else if (step.dex == 1) {
            // UNISWAP_V3
            _swapUniswapV3(step.data, amountIn, step.minOut);
        } else if (step.dex == 2) {
            // AERODROME
            _swapAerodrome(step.tokenIn, step.tokenOut, step.data, amountIn, step.minOut);
        } else if (step.dex == 6) {
            // AERODROME_SLIPSTREAM (nouveau)
            _swapAerodromeSlipstream(step.tokenIn, step.tokenOut, step.data, amountIn, step.minOut);
        } else if (step.dex == 3) {
            // CURVE
            _swapCurve(step.tokenIn, step.tokenOut, step.data, amountIn, step.minOut);
        } else if (step.dex == 4) {
            // BALANCER
            _swapBalancer(step.tokenIn, step.tokenOut, step.data, amountIn, step.minOut);
        } else if (step.dex == 5) {
            // SUSHISWAP
            _swapSushiswap(step.tokenIn, step.tokenOut, amountIn, step.minOut);
        } else {
            revert InvalidDex();
        }

        // Mesure réelle du montant reçu
        uint256 balanceAfter = _balanceOf(tokenOut);
        amountOut = balanceAfter - balanceBefore;

        if (amountOut < step.minOut) revert InsufficientOutput();
        
        emit SwapExecuted(step.dex, step.tokenIn, tokenOut, amountIn, amountOut);
    }

    // ========== DEX-SPECIFIC SWAP FUNCTIONS ==========
    
    function _swapUniswapV3(bytes memory path, uint256 amountIn, uint128 minOut) internal {
        _universalApprove(_getTokenFromPath(path), address(UNISWAP_V3_ROUTER), amountIn);
        
        ISwapRouter.ExactInputParams memory params = ISwapRouter.ExactInputParams({
            path: path,
            recipient: address(this),
            deadline: block.timestamp,
            amountIn: amountIn,
            amountOutMinimum: minOut
        });

        UNISWAP_V3_ROUTER.exactInput(params);
    }

    function _swapUniswapV2(address tokenIn, address tokenOut, uint256 amountIn, uint128 minOut) internal {
        _universalApprove(tokenIn, UNISWAP_V2_ROUTER, amountIn);
        
        address[] memory path = new address[](2);
        path[0] = tokenIn;
        path[1] = tokenOut;

        (bool success,) = UNISWAP_V2_ROUTER.call(
            abi.encodeWithSignature(
                "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
                amountIn, minOut, path, address(this), block.timestamp
            )
        );
        
        if (!success) revert TransferFailed();
    }

    function _swapAerodrome(address tokenIn, address tokenOut, bytes memory data, uint256 amountIn, uint128 minOut) internal {
        _universalApprove(tokenIn, AERODROME_ROUTER, amountIn);
        
        address[] memory route = new address[](2);
        route[0] = tokenIn;
        route[1] = tokenOut;

        (bool success,) = AERODROME_ROUTER.call(
            abi.encodeWithSignature(
                "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
                amountIn, minOut, route, address(this), block.timestamp
            )
        );

        if (!success) revert TransferFailed();
    }

    /// @notice Swap sur Aerodrome Slipstream (concentrated liquidity)
    function _swapAerodromeSlipstream(
        address tokenIn, 
        address tokenOut, 
        bytes memory data, 
        uint256 amountIn, 
        uint128 minOut
    ) internal {
        _universalApprove(tokenIn, AERODROME_SLIPSTREAM_ROUTER, amountIn);
        
        // Decode tick spacing
        int24 tickSpacing = abi.decode(data, (int24));
        
        // Call exactInputSingle
        (bool success,) = AERODROME_SLIPSTREAM_ROUTER.call(
            abi.encodeWithSignature(
                "exactInputSingle((address,address,int24,address,uint256,uint256,uint160))",
                tokenIn,
                tokenOut,
                tickSpacing,
                address(this),
                block.timestamp,
                amountIn,
                minOut,
                0 // sqrtPriceLimitX96
            )
        );
        
        require(success, "Slipstream swap failed");
    }

    function _swapCurve(address tokenIn, address tokenOut, bytes memory data, uint256 amountIn, uint128 minOut) internal {
        (address pool, int128 i, int128 j) = abi.decode(data, (address, int128, int128));
        
        _universalApprove(tokenIn, pool, amountIn);

        (bool success,) = pool.call(
            abi.encodeWithSignature(
                "exchange(int128,int128,uint256,uint256)",
                i, j, amountIn, minOut
            )
        );

        if (!success) revert TransferFailed();
    }

    function _swapBalancer(address tokenIn, address tokenOut, bytes memory data, uint256 amountIn, uint128 minOut) internal {
        _universalApprove(tokenIn, BALANCER_VAULT, amountIn);
        
        bytes32 poolId = abi.decode(data, (bytes32));

        (bool success,) = BALANCER_VAULT.call(
            abi.encodeWithSignature(
                "swap((bytes32,uint8,address,address,uint256,bytes),(address,bool,address,bool),uint256,uint256)",
                poolId, 0, tokenIn, tokenOut, amountIn, "",
                address(this), false, address(this), false,
                minOut, block.timestamp
            )
        );

        if (!success) revert TransferFailed();
    }

    function _swapSushiswap(address tokenIn, address tokenOut, uint256 amountIn, uint128 minOut) internal {
        _universalApprove(tokenIn, SUSHISWAP_ROUTER, amountIn);
        
        address[] memory path = new address[](2);
        path[0] = tokenIn;
        path[1] = tokenOut;

        (bool success,) = SUSHISWAP_ROUTER.call(
            abi.encodeWithSignature(
                "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
                amountIn, minOut, path, address(this), block.timestamp
            )
        );

        if (!success) revert TransferFailed();
    }

    // ========== UNIVERSAL TOKEN HELPERS ==========
    
    function _balanceOf(address token) internal view returns (uint256) {
        (bool success, bytes memory data) = token.staticcall(
            abi.encodeWithSelector(IERC20.balanceOf.selector, address(this))
        );
        
        if (!success || data.length == 0) {
            return 0;
        }
        
        return abi.decode(data, (uint256));
    }

    function _universalApprove(address token, address spender, uint256 amount) internal {
        (bool success, bytes memory data) = token.staticcall(
            abi.encodeWithSelector(IERC20.allowance.selector, address(this), spender)
        );
        
        uint256 currentAllowance = success && data.length >= 32 ? abi.decode(data, (uint256)) : 0;

        if (currentAllowance >= amount) return;

        // Reset pour USDT-like
        if (currentAllowance > 0) {
            (bool resetSuccess, ) = token.call(
                abi.encodeWithSelector(IERC20.approve.selector, spender, 0)
            );
            require(resetSuccess, "Reset approve failed");
        }

        // Approve infinite
        (bool approveSuccess, bytes memory approveData) = token.call(
            abi.encodeWithSelector(IERC20.approve.selector, spender, type(uint256).max)
        );
        
        require(approveSuccess, "Approve failed");
        
        if (approveData.length > 0) {
            require(abi.decode(approveData, (bool)), "Approve returned false");
        }
    }

    function _universalTransfer(address token, address to, uint256 amount) internal {
        (bool success, bytes memory data) = token.call(
            abi.encodeWithSelector(IERC20.transfer.selector, to, amount)
        );
        
        require(success, "Transfer failed");
        
        if (data.length > 0) {
            require(abi.decode(data, (bool)), "Transfer returned false");
        }
    }

    function _getTokenFromPath(bytes memory path) internal pure returns (address token) {
        assembly {
            token := mload(add(path, 20))
        }
    }

    function _updateMetrics(address asset, uint256 profit) internal {
        unchecked {
            ++totalTrades;
            profitByToken[asset] += profit;
            totalProfitWei += profit;
        }
    }

    // ========== ADMIN FUNCTIONS ==========
    
    function pause() external onlyOwner {
        _pause();
    }

    function unpause() external onlyOwner {
        _unpause();
    }

    function setAuthorized(address caller, bool status) external onlyOwner {
        authorized[caller] = status;
        emit CallerUpdated(caller, status);
    }

    function setSupportedAsset(address asset, bool status) external onlyOwner {
        supportedAssets[asset] = status;
    }

    function setMaxTradesPerHour(uint32 limit) external onlyOwner {
        maxTradesPerHour = limit;
    }
    
    function setMaxPriorityFee(uint128 maxFeeGwei) external onlyOwner {
        maxPriorityFeeGwei = maxFeeGwei;
    }
    
    function setMinProfitAfterGas(uint128 minProfit) external onlyOwner {
        minProfitAfterGas = minProfit;
    }
    
    function setPreferredFlashLoanProvider(address provider) external onlyOwner {
        preferredFlashLoanProvider = provider;
    }

    function resetRateLimit() external onlyOwner {
        tradesThisHour = 0;
        lastHourStart = uint48(block.timestamp);
    }

    function rescue(address token, uint256 amount) external onlyOwner {
        if (amount == type(uint256).max) {
            amount = _balanceOf(token);
        }
        _universalTransfer(token, owner(), amount);
        emit EmergencyWithdraw(token, amount);
    }

    function rescueETH() external onlyOwner {
        (bool success,) = payable(owner()).call{value: address(this).balance}("");
        require(success, "ETH transfer failed");
    }

    function batchApprove(address[] calldata tokens, address[] calldata spenders) external onlyOwner {
        unchecked {
            for (uint256 i; i < tokens.length; ++i) {
                for (uint256 j; j < spenders.length; ++j) {
                    if (spenders[j] != address(0)) {
                        _universalApprove(tokens[i], spenders[j], type(uint256).max);
                    }
                }
            }
        }
    }
    
    function clearExecutedTxs(bytes32[] calldata txHashes) external onlyOwner {
        for (uint256 i; i < txHashes.length; ++i) {
            executedTxs[txHashes[i]] = false;
        }
    }
    
    function withdrawProfits(address[] calldata tokens) external onlyOwner {
        for (uint256 i; i < tokens.length; ++i) {
            uint256 balance = _balanceOf(tokens[i]);
            if (balance > 0) {
                _universalTransfer(tokens[i], owner(), balance);
            }
        }
    }

    // ========== VIEW FUNCTIONS ==========
    
    function getMetrics() external view returns (
        uint256 trades,
        uint256 totalProfit,
        uint256 tradesRemaining
    ) {
        unchecked {
            uint256 remaining = (block.timestamp >= lastHourStart + 1 hours) 
                ? maxTradesPerHour 
                : maxTradesPerHour - tradesThisHour;
                
            return (totalTrades, totalProfitWei, remaining);
        }
    }

    function getProfitByToken(address token) external view returns (uint256) {
        return profitByToken[token];
    }

    function isAuthorized(address caller) external view returns (bool) {
        return authorized[caller];
    }

    function getBalance(address token) external view returns (uint256) {
        return _balanceOf(token);
    }
    
    function estimateNetProfit(
        uint256 grossProfit,
        uint256 flashLoanAmount,
        uint256 flashLoanFee,
        uint256 estimatedGasUnits,
        uint256 gasPriceGwei
    ) external pure returns (int256 netProfit) {
        uint256 flashLoanCost = (flashLoanAmount * flashLoanFee) / 10000;
        uint256 gasCost = estimatedGasUnits * gasPriceGwei * 1 gwei;
        
        if (grossProfit > flashLoanCost + gasCost) {
            return int256(grossProfit - flashLoanCost - gasCost);
        } else {
            return -int256(flashLoanCost + gasCost - grossProfit);
        }
    }
    
    function calculateMinProfitRequired(
        uint256 flashLoanAmount,
        uint256 flashLoanFee,
        uint256 estimatedGasUnits,
        uint256 gasPriceGwei
    ) external pure returns (uint256 minProfit) {
        uint256 flashLoanCost = (flashLoanAmount * flashLoanFee) / 10000;
        uint256 gasCost = estimatedGasUnits * gasPriceGwei * 1 gwei;
        return flashLoanCost + gasCost;
    }
    
    function isExecuted(bytes32 txHash) external view returns (bool) {
        return executedTxs[txHash];
    }
    
    function getLastExecutionBlock(address caller) external view returns (uint256) {
        return lastExecutionBlock[caller];
    }
    
    function getMEVSettings() external view returns (uint128 maxPriorityFee, uint128 minProfit) {
        return (maxPriorityFeeGwei, minProfitAfterGas);
    }

    // Aave interface
    function ADDRESSES_PROVIDER() external view override returns (IPoolAddressesProvider) {
        return IPoolAddressesProvider(address(0));
    }

    function POOL() external view override returns (IPool) {
        return AAVE_POOL;
    }

    receive() external payable {}
}
