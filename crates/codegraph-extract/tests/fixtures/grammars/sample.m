#import <Foundation/Foundation.h>
@interface Greeter : NSObject
@property (nonatomic, copy) NSString *name;
- (void)greet:(NSString *)who times:(int)n;
@end
@implementation Greeter
- (void)greet:(NSString *)who times:(int)n { [self log:who]; printf("%d", n); }
@end
