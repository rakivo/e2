#[macro_export]
macro_rules! tprint {
    ($scratch:expr, $($tt:tt)*) => {
        #[allow(unused, dead_code)]
        {
            use core::fmt::Write as _;

            $scratch.clear();
            _ = write!(&mut $scratch, $($tt)*);

            $scratch
        }
    };
}
